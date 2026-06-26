//! Agent subprocess runner. Design: revised issue model.
//!
//! auwsx spawns an agent CLI as a DIRECT child process (`tokio::process`) and
//! owns it — there is no tmux indirection (tmux is human-only spectating, layered
//! on later and optional). The agent does its phase work and reports back by
//! invoking the `auwsx` control CLI, which sets the issue's status; that status,
//! read on the next scheduler tick, decides what happens next. The agent's exit
//! is incidental — see `crate::state`.
//!
//! A command template (e.g. `projects.work_agent_cmd`) is a whitespace-separated
//! argv with a `{prompt}` placeholder:
//!   * if a token contains `{prompt}`, the prompt is substituted there (one arg);
//!   * if there is no `{prompt}` token, the prompt is fed on the child's stdin
//!     (the `echo "<prompt>" | opencode run …` shape, minus the shell).
//!
//! No shell is involved, so prompt content can never inject arguments. The
//! tradeoff: templates cannot use shell features (pipes/quotes) — keep them a
//! plain flag list plus `{prompt}`. Per-agent default templates live in the
//! `claude` / `codex` / `opencode` submodules.

pub mod claude;
pub mod codex;
pub mod opencode;

use crate::Result;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command as ChildCommand;

/// How an agent process ended. Ids match the `agent_runs.exit_kind` CHECK
/// domain in `0001_init.sql`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitKind {
    /// Ran to completion (any exit code; inspect `exit_code`).
    Exited,
    /// Exceeded the deadline and was killed by auwsx.
    Timeout,
    /// Killed by a signal (not auwsx's timeout).
    Killed,
    /// Never started (spawn failed — e.g. binary not found).
    Error,
}

impl ExitKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExitKind::Exited => "exited",
            ExitKind::Timeout => "timeout",
            ExitKind::Killed => "killed",
            ExitKind::Error => "error",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "exited" => ExitKind::Exited,
            "timeout" => ExitKind::Timeout,
            "killed" => ExitKind::Killed,
            "error" => ExitKind::Error,
            _ => return None,
        })
    }
}

/// What to spawn and how to constrain it.
#[derive(Debug, Clone)]
pub struct AgentSpec<'a> {
    /// Whitespace-separated argv template with an optional `{prompt}` token.
    pub cmd_template: &'a str,
    /// The prompt: substituted into `{prompt}`, or piped to stdin if absent.
    pub prompt: &'a str,
    /// Working directory for the child (the issue's worktree).
    pub cwd: &'a Path,
    /// Combined stdout+stderr are captured here.
    pub log_path: &'a Path,
    /// Hard deadline; on expiry the child is killed and `Timeout` returned.
    pub timeout: Duration,
    /// Extra environment for the child. Issue workers receive an issue-local
    /// control outbox, not unrestricted daemon socket access.
    pub env: &'a [(String, String)],
}

/// Result of one agent run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOutcome {
    pub exit_kind: ExitKind,
    /// Process exit code when it exited normally; `None` if signaled/timed-out.
    pub exit_code: Option<i32>,
    /// Child PID, if it was spawned.
    pub pid: Option<u32>,
}

/// Split a template into argv, substituting `{prompt}`. Returns the argv and
/// whether the prompt still needs to go on stdin (no `{prompt}` token present).
pub fn build_argv(cmd_template: &str, prompt: &str) -> Result<(Vec<String>, bool)> {
    let mut argv = Vec::new();
    let mut substituted = false;
    for tok in cmd_template.split_whitespace() {
        if tok.contains("{prompt}") {
            argv.push(tok.replace("{prompt}", prompt));
            substituted = true;
        } else {
            argv.push(tok.to_string());
        }
    }
    if argv.is_empty() {
        return Err(anyhow!("empty agent command template"));
    }
    Ok((argv, !substituted))
}

#[derive(Debug, Clone, Copy)]
pub struct AgentTemplateVars<'a> {
    pub daemon_socket: Option<&'a Path>,
    pub control_dir: Option<&'a Path>,
}

impl<'a> AgentTemplateVars<'a> {
    pub fn main_job(daemon_socket: &'a Path) -> Self {
        Self {
            daemon_socket: Some(daemon_socket),
            control_dir: None,
        }
    }

    pub fn issue(daemon_socket: &'a Path, control_dir: &'a Path) -> Self {
        Self {
            daemon_socket: Some(daemon_socket),
            control_dir: Some(control_dir),
        }
    }
}

/// Expand auwsx-owned placeholders before argv splitting. These are not shell
/// variables: templates are split directly and never run through a shell.
pub fn expand_cmd_template(cmd_template: &str, vars: AgentTemplateVars<'_>) -> String {
    let socket_dir = vars
        .daemon_socket
        .and_then(Path::parent)
        .map(|path| path.to_string_lossy().to_string())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| ".".to_string());
    let control_dir = vars
        .control_dir
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| socket_dir.clone());
    cmd_template
        .replace("{auwsx_socket_dir}", &socket_dir)
        .replace("{auwsx_control_dir}", &control_dir)
}

/// Spawn the agent, capture output to `log_path`, and wait (bounded by
/// `timeout`). A spawn failure is returned as an `Error` outcome (so the caller
/// can record it to `agent_runs`), not a hard error; only set-up I/O failures
/// (creating the log file / its parent) propagate as `Err`.
pub async fn run(spec: AgentSpec<'_>) -> Result<AgentOutcome> {
    let (argv, prompt_on_stdin) = build_argv(spec.cmd_template, spec.prompt)?;

    if let Some(parent) = spec.log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating log dir {}", parent.display()))?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(spec.log_path)
        .with_context(|| format!("opening log file {}", spec.log_path.display()))?;
    let log_err = log
        .try_clone()
        .context("cloning log file handle for stderr")?;

    let mut cmd = ChildCommand::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(spec.cwd)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .stdin(if prompt_on_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);
    for (k, v) in spec.env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Record the failure in the log too, for transparency.
            let _ = std::fs::write(
                spec.log_path,
                format!("auwsx: failed to spawn {:?}: {e}\n", argv[0]),
            );
            return Ok(AgentOutcome {
                exit_kind: ExitKind::Error,
                exit_code: None,
                pid: None,
            });
        }
    };
    let pid = child.id();

    if prompt_on_stdin {
        if let Some(mut stdin) = child.stdin.take() {
            // Best-effort: a broken pipe (agent that ignores stdin) is not fatal.
            let _ = stdin.write_all(spec.prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
    }

    match tokio::time::timeout(spec.timeout, child.wait()).await {
        Ok(Ok(status)) => Ok(AgentOutcome {
            exit_kind: classify(&status),
            exit_code: status.code(),
            pid,
        }),
        Ok(Err(e)) => Err(anyhow!("waiting on agent process: {e}")),
        Err(_elapsed) => {
            // Deadline hit: kill and reap so we don't leak a zombie.
            let _ = child.start_kill();
            let _ = child.wait().await;
            Ok(AgentOutcome {
                exit_kind: ExitKind::Timeout,
                exit_code: None,
                pid,
            })
        }
    }
}

/// A normal exit (code present) is `Exited`; a `None` code means the process was
/// terminated by a signal we didn't issue → `Killed`.
fn classify(status: &std::process::ExitStatus) -> ExitKind {
    if status.code().is_some() {
        ExitKind::Exited
    } else {
        ExitKind::Killed
    }
}

/// Port the pipeline spawns through. The production adapter ([`SubprocessExecutor`])
/// delegates to [`run`]; tests substitute a fake that reads `AUWSX_ISSUE_ID` from
/// `spec.env` and applies the status change a real agent would make via the
/// control CLI — making the whole drive loop deterministic without real agents.
#[async_trait]
pub trait AgentExecutor: Send + Sync {
    async fn execute(&self, spec: AgentSpec<'_>) -> Result<AgentOutcome>;
}

/// Production executor: spawns the real agent process.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubprocessExecutor;

#[async_trait]
impl AgentExecutor for SubprocessExecutor {
    async fn execute(&self, spec: AgentSpec<'_>) -> Result<AgentOutcome> {
        run(spec).await
    }
}

/// Convenience: a shared production executor behind the port.
pub fn subprocess_executor() -> std::sync::Arc<dyn AgentExecutor> {
    std::sync::Arc::new(SubprocessExecutor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // build_argv is pure; its substitution/stdin contract plus the spawn
    // outcomes (exit/timeout/spawn-error) are exercised in tests/agent.rs
    // against real `sh`/`sleep` commands.

    #[test]
    fn given_socket_dir_placeholder_when_expanded_then_uses_parent_dir() {
        let got = expand_cmd_template(
            "codex exec --add-dir {auwsx_socket_dir} --json {prompt}",
            AgentTemplateVars::main_job(Path::new("/x/y/auwsx.sock")),
        );

        assert_eq!(got, "codex exec --add-dir /x/y --json {prompt}");
    }

    #[test]
    fn given_control_dir_placeholder_when_expanded_then_uses_issue_control_dir() {
        let got = expand_cmd_template(
            "codex exec --add-dir {auwsx_control_dir} --json {prompt}",
            AgentTemplateVars::issue(
                Path::new("/daemon/auwsx.sock"),
                Path::new("/worktree/.auwsx/control"),
            ),
        );

        assert_eq!(
            got,
            "codex exec --add-dir /worktree/.auwsx/control --json {prompt}"
        );
    }

    #[test]
    fn given_no_socket_parent_when_expanded_then_uses_current_dir() {
        let got = expand_cmd_template(
            "cmd {auwsx_socket_dir}",
            AgentTemplateVars::main_job(Path::new("auwsx.sock")),
        );

        assert_eq!(got, "cmd .");
    }
}

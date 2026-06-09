//! Pipeline: the per-phase execution of an actionable issue.
//!
//! The pipeline is a state machine, NOT one mega-prompt. auwsx owns transitions,
//! worktree/log I/O, and agent invocation (deterministic); the agent owns the
//! cognitive work and reports back via the control CLI. auwsx never parses prose
//! — the status the agent sets is the only signal (see `crate::state`).
//!
//! [`plan_phase`] is pure (status → role + worktree-need). [`execute`] runs one
//! phase: ensure the worktree, build the prompt + env, spawn the agent through
//! the [`AgentExecutor`] port, and record the run to `agent_runs`. The status
//! the agent leaves behind drives the next scheduler tick.
//!
//! Phase → role (templates from `projects.*_agent_cmd`):
//!
//! ```text
//!   CONSOLIDATING  main    (no worktree)
//!   PLANNING       plan    (worktree created here)
//!   IMPLEMENTING   work
//!   REVIEW         review  (fresh session; falls back to work)
//!   NEEDS_FIX      work
//!   AUDIT          work
//!   CONFLICTED     work
//!   COMPLETING     work
//! ```

use crate::agent::{AgentExecutor, AgentSpec};
use crate::clock::Clock;
use crate::db::agent_runs::{self, Role, StartRun};
use crate::db::{findings, issues, projects, subtasks, Db};
use crate::events::Event;
use crate::prompt::{self, PromptContext};
use crate::state::IssueStatus;
use crate::steering;
use crate::worktree::{branch_for_issue, Worktrees};
use crate::Result;
use anyhow::{anyhow, Context};
use directories::ProjectDirs;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::broadcast;

/// Pure phase plan: which agent role runs this status, and whether it needs a
/// worktree. `None` iff the status is not actionable (the scheduler never
/// dispatches those). The `Some` set equals `IssueStatus::is_actionable`.
pub fn plan_phase(status: IssueStatus) -> Option<(Role, bool)> {
    use IssueStatus::*;
    Some(match status {
        Consolidating => (Role::Main, false),
        Planning => (Role::Plan, true),
        Implementing | NeedsFix | Audit | Conflicted | Completing => (Role::Work, true),
        Review => (Role::Review, true),
        _ => return None,
    })
}

/// Ports + handles one phase execution needs. Borrowed so the scheduler can
/// hold the owned versions (behind `Arc`) and lend them per tick.
pub struct Deps<'a> {
    pub db: &'a Db,
    pub clock: &'a dyn Clock,
    pub executor: &'a dyn AgentExecutor,
    pub worktrees: &'a dyn Worktrees,
    pub events: &'a broadcast::Sender<Event>,
    /// Socket path injected as `AUWSX_SOCK` so the agent's control CLI finds us.
    pub socket: PathBuf,
}

/// Execute one phase for `issue_id`: ensure worktree → build prompt + env →
/// spawn agent → record the run. The agent advances the issue itself via the
/// control CLI, so this does not change status on the happy path; it only reads
/// the resulting status to log `status_after`.
///
/// A no-op (Ok) if the issue vanished or is no longer actionable by the time we
/// run (it may have been advanced/aborted between tick and dispatch).
pub async fn execute(deps: &Deps<'_>, issue_id: i64) -> Result<()> {
    let pool = deps.db.pool();

    let Some(issue) = issues::get(pool, issue_id).await? else {
        return Ok(());
    };
    let Some((role, needs_worktree)) = plan_phase(issue.status) else {
        return Ok(());
    };
    let project = projects::get(pool, issue.project_id)
        .await?
        .ok_or_else(|| anyhow!("issue {issue_id} references missing project {}", issue.project_id))?;

    // Worktree: created once, at the first phase that needs one (PLANNING).
    let mut worktree_path = issue.worktree_path.clone();
    if needs_worktree && worktree_path.is_none() {
        let branch = branch_for_issue(issue_id);
        let handle = deps
            .worktrees
            .create(&project, &branch)
            .await
            .with_context(|| format!("creating worktree for issue {issue_id}"))?;
        let path_str = handle.path.to_string_lossy().to_string();
        issues::set_worktree(
            pool,
            issue_id,
            Some(&handle.branch),
            Some(&path_str),
            None,
            deps.clock.now_ms(),
        )
        .await?;
        worktree_path = Some(path_str);
    }

    // CONSOLIDATING runs at the repo root (no worktree yet); later phases run in
    // the worktree.
    let cwd = PathBuf::from(worktree_path.clone().unwrap_or_else(|| project.repo_path.clone()));

    // Context for the prompt.
    let subtasks = subtasks::list_by_issue(pool, issue_id).await?;
    let steering = steering::list_pending(pool, issue_id).await?;
    let open_findings = findings::list_open(pool, issue_id).await?;
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &subtasks,
        steering: &steering,
        open_findings: &open_findings,
    };
    let Some(prompt_text) = prompt::build(&ctx) else {
        return Ok(());
    };

    let spawned_at = deps.clock.now_ms();
    let (log_path, prompt_path) = run_paths(issue_id, spawned_at)?;
    std::fs::write(&prompt_path, &prompt_text)
        .with_context(|| format!("writing prompt to {}", prompt_path.display()))?;

    let cmd_template = project.agent_cmd_for(role).to_string();
    let env = vec![
        ("AUWSX_SOCK".to_string(), deps.socket.to_string_lossy().to_string()),
        ("AUWSX_ISSUE_ID".to_string(), issue_id.to_string()),
        ("AUWSX_AGENT_ROLE".to_string(), role.as_str().to_string()),
    ];

    let log_str = log_path.to_string_lossy().to_string();
    let prompt_str = prompt_path.to_string_lossy().to_string();
    let run_id = agent_runs::start(
        pool,
        StartRun {
            issue_id: Some(issue_id),
            main_job_id: None,
            role,
            phase: issue.status.as_str(),
            agent_cmd: &cmd_template,
            status_before: Some(issue.status.as_str()),
            pid: None,
            prompt_path: Some(&prompt_str),
            log_path: Some(&log_str),
        },
        spawned_at,
    )
    .await?;

    let timeout = Duration::from_secs((project.iteration_timeout_min.max(1) as u64) * 60);
    let outcome = deps
        .executor
        .execute(AgentSpec {
            cmd_template: &cmd_template,
            prompt: &prompt_text,
            cwd: &cwd,
            log_path: &log_path,
            timeout,
            env: &env,
        })
        .await?;

    // The agent (or the test fake) may have advanced the status via the control
    // CLI during the run; reload to record where it landed.
    let status_after = issues::get(pool, issue_id)
        .await?
        .map(|i| i.status.as_str().to_string());
    agent_runs::finish(
        pool,
        run_id,
        status_after.as_deref(),
        outcome.exit_code.map(|c| c as i64),
        outcome.exit_kind,
        deps.clock.now_ms(),
        None,
    )
    .await?;

    Ok(())
}

/// Per-run log + prompt artifact paths under the daemon data dir (NOT in the
/// repo/worktree — those hold the agent's own `.auwsx/` artifacts). Creates the
/// parent directory.
fn run_paths(issue_id: i64, spawned_at: i64) -> Result<(PathBuf, PathBuf)> {
    let base = data_dir().join("runs").join(format!("issue-{issue_id}"));
    std::fs::create_dir_all(&base)
        .with_context(|| format!("creating run dir {}", base.display()))?;
    Ok((
        base.join(format!("run-{spawned_at}.log")),
        base.join(format!("run-{spawned_at}.prompt.txt")),
    ))
}

fn data_dir() -> PathBuf {
    if let Ok(env) = std::env::var("AUWSX_DATA_DIR") {
        return PathBuf::from(env);
    }
    if let Some(dirs) = ProjectDirs::from("", "", "auwsx") {
        return dirs.data_dir().to_path_buf();
    }
    std::env::temp_dir().join("auwsx")
}

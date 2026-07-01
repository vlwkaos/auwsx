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
//!   NEW / PLANNING     plan    (worktree created here)
//!   WORKING            work
//!   REVIEWING          review  (fresh session; falls back to work)
//!   FIXING             work
//!   AUDITING           work
//!   RESOLVING_CONFLICT work
//!   MERGING            work
//! ```

use crate::agent::{self, AgentExecutor, AgentSpec, ExitKind};
use crate::artifacts;
use crate::clock::Clock;
use crate::control_outbox::{self, ControlSnapshot};
use crate::db::agent_runs::{self, Role, StartRun};
use crate::db::{findings, global_settings, issues, projects, subtasks, Db};
use crate::events::Event;
use crate::prompt::{self, PromptContext};
use crate::state::IssueStatus;
use crate::steering;
use crate::worktree::{branch_for_issue, Worktrees};
use crate::Result;
use anyhow::{anyhow, Context};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::broadcast;

const ISSUE_REPORT_LIMIT: usize = 16 * 1024;
const PHASE_REPORT_FILE: &str = "phase-report.md";

/// Pure phase plan: which agent role runs this status, and whether it needs a
/// worktree. `None` iff the status is not actionable (the scheduler never
/// dispatches those). The `Some` set equals `IssueStatus::is_actionable`.
pub fn plan_phase(status: IssueStatus) -> Option<(Role, bool)> {
    use IssueStatus::*;
    Some(match status {
        New | Planning => (Role::Plan, true),
        Working | Fixing | Auditing | ResolvingConflict | Merging => (Role::Work, true),
        Reviewing => (Role::Review, true),
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
    /// Daemon socket path recorded in logs; issue workers use the control outbox.
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

    let Some(mut issue) = issues::get(pool, issue_id).await? else {
        return Ok(());
    };
    let Some((role, needs_worktree)) = plan_phase(issue.status) else {
        return Ok(());
    };
    let project = projects::get(pool, issue.project_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "issue {issue_id} references missing project {}",
                issue.project_id
            )
        })?;

    if issue.status == IssueStatus::New {
        let entered_at = deps.clock.now_ms();
        issues::transition(pool, issue_id, IssueStatus::Planning, entered_at).await?;
        issue.status = IssueStatus::Planning;
        let _ = deps.events.send(Event::IssueStatus {
            issue_id,
            status: IssueStatus::Planning,
        });
    }

    // Worktree: created once, at the first phase that needs one (PLANNING).
    let mut worktree_path = issue.worktree_path.clone();
    if needs_worktree && worktree_path.is_none() {
        let branch = branch_for_issue(issue_id);
        let handle = match deps
            .worktrees
            .create(&project, &branch)
            .await
            .with_context(|| format!("creating worktree for issue {issue_id}"))
        {
            Ok(handle) => handle,
            Err(e) => {
                let failed_at = deps.clock.now_ms();
                mark_issue_failed_with_setup_log(
                    deps,
                    pool,
                    &project,
                    issue_id,
                    role,
                    issue.status.as_str(),
                    &e,
                    failed_at,
                )
                .await?;
                return Err(e);
            }
        };
        let path_str = handle.path.to_string_lossy().to_string();
        if let Err(e) = issues::set_worktree(
            pool,
            issue_id,
            Some(&handle.branch),
            Some(&path_str),
            deps.clock.now_ms(),
        )
        .await
        {
            let cleanup_result = deps.worktrees.teardown(&project, &handle).await;
            let failed_at = deps.clock.now_ms();
            mark_issue_failed_with_setup_log(
                deps,
                pool,
                &project,
                issue_id,
                role,
                issue.status.as_str(),
                &e,
                failed_at,
            )
            .await?;
            cleanup_result.with_context(|| {
                format!("cleaning up worktree after failed DB record for issue {issue_id}")
            })?;
            return Err(e).with_context(|| format!("recording worktree for issue {issue_id}"));
        }
        worktree_path = Some(path_str);
    }

    let cwd = PathBuf::from(
        worktree_path
            .clone()
            .unwrap_or_else(|| project.repo_path.clone()),
    );

    // Context for the prompt.
    let subtasks = subtasks::list_by_issue(pool, issue_id).await?;
    let steering = steering::list_pending(pool, issue_id).await?;
    let steering_snapshot_ids: Vec<i64> = steering.iter().map(|item| item.id).collect();
    let open_findings = findings::list_open(pool, issue_id).await?;
    let global_settings = global_settings::get(pool).await?;
    let agent_cmd = project.agent_cmd_for(role);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &subtasks,
        steering: &steering,
        open_findings: &open_findings,
        pipeline_ux_guidance: Some(&global_settings.pipeline_ux_guidance),
        memory_invocation: prompt::MemoryInvocation::from_agent_cmd(agent_cmd),
    };
    let Some(prompt_text) = prompt::build(&ctx) else {
        return Ok(());
    };

    let spawned_at = deps.clock.now_ms();
    let (log_path, prompt_path) = match artifacts::issue_run_paths(issue_id, spawned_at) {
        Ok(paths) => paths,
        Err(e) => {
            mark_issue_failed(pool, deps.events, issue_id, deps.clock.now_ms()).await?;
            return Err(e).context("preparing issue run artifact paths");
        }
    };
    if let Err(e) = std::fs::write(&prompt_path, &prompt_text) {
        let error = anyhow!(e);
        mark_issue_failed_with_setup_log(
            deps,
            pool,
            &project,
            issue_id,
            role,
            issue.status.as_str(),
            &error,
            deps.clock.now_ms(),
        )
        .await?;
        return Err(error).with_context(|| format!("writing prompt to {}", prompt_path.display()));
    }
    let control_dir = cwd.join(".auwsx").join("control");
    if let Err(e) = std::fs::create_dir_all(&control_dir) {
        let error = anyhow!(e);
        mark_issue_failed_with_setup_log(
            deps,
            pool,
            &project,
            issue_id,
            role,
            issue.status.as_str(),
            &error,
            deps.clock.now_ms(),
        )
        .await?;
        return Err(error)
            .with_context(|| format!("creating control dir {}", control_dir.display()));
    }
    let outbox_path = control_dir.join(format!("run-{spawned_at}.jsonl"));
    let snapshot_path = control_dir.join(format!("run-{spawned_at}.snapshot.json"));
    let control_snapshot = ControlSnapshot {
        issue: issue.clone(),
        subtasks: subtasks.clone(),
        findings: open_findings.clone(),
        steering: steering.clone(),
    };
    if let Err(e) = control_outbox::write_snapshot(&snapshot_path, &control_snapshot) {
        mark_issue_failed_with_setup_log(
            deps,
            pool,
            &project,
            issue_id,
            role,
            issue.status.as_str(),
            &e,
            deps.clock.now_ms(),
        )
        .await?;
        return Err(e).with_context(|| {
            format!("writing issue control snapshot {}", snapshot_path.display())
        });
    }

    let cmd_template = agent::expand_cmd_template(
        agent_cmd,
        agent::AgentTemplateVars::issue(&deps.socket, &control_dir),
    );
    let env = vec![
        (
            "AUWSX_BIN".to_string(),
            std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "auwsx".to_string()),
        ),
        ("AUWSX_ISSUE_ID".to_string(), issue_id.to_string()),
        ("AUWSX_PROJECT_ID".to_string(), issue.project_id.to_string()),
        ("AUWSX_AGENT_ROLE".to_string(), role.as_str().to_string()),
        (
            control_outbox::OUTBOX_ENV.to_string(),
            outbox_path.to_string_lossy().to_string(),
        ),
        (
            control_outbox::SNAPSHOT_ENV.to_string(),
            snapshot_path.to_string_lossy().to_string(),
        ),
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
    append_system_event(
        &log_path,
        serde_json::json!({
            "kind": "spawn",
            "run_id": run_id,
            "issue_id": issue_id,
            "role": role.as_str(),
            "phase": issue.status.as_str(),
            "cmd": cmd_template,
            "cwd": cwd.to_string_lossy(),
            "socket": deps.socket.to_string_lossy(),
            "control_outbox": outbox_path.to_string_lossy(),
            "control_snapshot": snapshot_path.to_string_lossy(),
            "prompt_path": prompt_str,
        }),
    );

    let timeout = Duration::from_secs((project.iteration_timeout_min.max(1) as u64) * 60);
    let execute_result = deps
        .executor
        .execute(AgentSpec {
            cmd_template: &cmd_template,
            prompt: &prompt_text,
            cwd: &cwd,
            log_path: &log_path,
            timeout,
            env: &env,
        })
        .await;
    let outcome = match execute_result {
        Ok(outcome) => outcome,
        Err(e) => {
            let failed_at = deps.clock.now_ms();
            let transition_result =
                issues::transition(pool, issue_id, IssueStatus::Failed, failed_at).await;
            let status_after = issues::get(pool, issue_id)
                .await?
                .map(|i| i.status.as_str().to_string());
            let note = match &transition_result {
                Ok(()) => e.to_string(),
                Err(status_error) => {
                    format!("{e}; additionally failed to mark issue FAILED: {status_error:#}")
                }
            };
            append_system_event(
                &log_path,
                serde_json::json!({
                    "kind": "finish",
                    "run_id": run_id,
                    "issue_id": issue_id,
                    "result": "executor_error",
                    "exit_kind": ExitKind::Error.as_str(),
                    "status_after": status_after.clone(),
                    "error": note,
                }),
            );
            agent_runs::finish(
                pool,
                run_id,
                status_after.as_deref(),
                None,
                ExitKind::Error,
                failed_at,
                Some(&note),
            )
            .await?;
            transition_result?;
            let _ = deps.events.send(Event::IssueStatus {
                issue_id,
                status: IssueStatus::Failed,
            });
            return Err(e);
        }
    };
    if let Some(pid) = outcome.pid {
        agent_runs::set_pid(pool, run_id, pid as i64).await?;
    }

    let replay_result = control_outbox::replay(
        deps.db,
        deps.events,
        issue_id,
        &control_snapshot,
        &outbox_path,
        deps.clock.now_ms(),
    )
    .await
    .and_then(|responses| {
        for resp in &responses {
            if let crate::ipc::Response::Err { message } = resp {
                anyhow::bail!("control outbox replay failed: {message}");
            }
        }
        Ok(responses)
    });
    let replay_responses = match replay_result {
        Ok(responses) => responses,
        Err(e) => {
            let failed_at = deps.clock.now_ms();
            let transition_result =
                issues::transition(pool, issue_id, IssueStatus::Failed, failed_at).await;
            let status_after = issues::get(pool, issue_id)
                .await?
                .map(|i| i.status.as_str().to_string());
            let note = e.to_string();
            append_system_event(
                &log_path,
                serde_json::json!({
                    "kind": "finish",
                    "run_id": run_id,
                    "issue_id": issue_id,
                    "result": "control_replay_error",
                    "exit_kind": ExitKind::Error.as_str(),
                    "exit_code": outcome.exit_code,
                    "status_after": status_after.clone(),
                    "error": note,
                }),
            );
            agent_runs::finish(
                pool,
                run_id,
                status_after.as_deref(),
                outcome.exit_code.map(|c| c as i64),
                ExitKind::Error,
                failed_at,
                Some(&note),
            )
            .await?;
            if transition_result.is_ok() {
                let _ = deps.events.send(Event::IssueStatus {
                    issue_id,
                    status: IssueStatus::Failed,
                });
            }
            transition_result?;
            return Err(e);
        }
    };
    append_system_event(
        &log_path,
        serde_json::json!({
            "kind": "control_replay",
            "run_id": run_id,
            "issue_id": issue_id,
            "commands": replay_responses.len(),
            "outbox": outbox_path.to_string_lossy(),
        }),
    );

    // The agent (or the test fake) may have advanced the status via the control
    // CLI during the run; reload to record where it landed.
    let mut status_after = issues::get(pool, issue_id)
        .await?
        .map(|i| i.status.as_str().to_string());
    let status_unchanged = status_after.as_deref() == Some(issue.status.as_str());
    let mut finish_note = None;
    if status_unchanged {
        mark_issue_failed(pool, deps.events, issue_id, deps.clock.now_ms()).await?;
        status_after = Some(IssueStatus::Failed.as_str().to_string());
        finish_note = Some(
            if outcome.exit_kind == ExitKind::Exited && outcome.exit_code == Some(0) {
                "agent exited without changing issue status; marked FAILED".to_string()
            } else {
                "agent exited unsuccessfully without changing issue status; marked FAILED"
                    .to_string()
            },
        );
    }
    if let Err(e) =
        steering::consume_ids(pool, issue_id, &steering_snapshot_ids, deps.clock.now_ms()).await
    {
        append_system_event(
            &log_path,
            serde_json::json!({
                "kind": "queue_consume_failed",
                "run_id": run_id,
                "issue_id": issue_id,
                "message": e.to_string(),
            }),
        );
    }
    if let Some(current) = issues::get(pool, issue_id).await? {
        if current.has_pending_steering && current.status == IssueStatus::ReadyToMerge {
            issues::transition(pool, issue_id, IssueStatus::Working, deps.clock.now_ms()).await?;
            status_after = Some(IssueStatus::Working.as_str().to_string());
            append_system_event(
                &log_path,
                serde_json::json!({
                    "kind": "queue_rework",
                    "run_id": run_id,
                    "issue_id": issue_id,
                    "from": IssueStatus::ReadyToMerge.as_str(),
                    "to": IssueStatus::Working.as_str(),
                }),
            );
            let _ = deps.events.send(Event::IssueStatus {
                issue_id,
                status: IssueStatus::Working,
            });
        }
    }
    let phase_report_path = cwd.join(".auwsx").join(PHASE_REPORT_FILE);
    let read_phase_report = read_phase_report(&cwd);
    let generated_phase_report = read_phase_report.is_none();
    let phase_report = read_phase_report.unwrap_or_else(|| {
        fallback_phase_report(
            role,
            issue.status.as_str(),
            &outcome.exit_kind,
            outcome.exit_code,
            finish_note.as_deref(),
        )
    });
    agent_runs::set_phase_report(pool, run_id, &phase_report).await?;
    append_system_event(
        &log_path,
        serde_json::json!({
            "kind": "phase_report",
            "run_id": run_id,
            "issue_id": issue_id,
            "path": phase_report_path.to_string_lossy(),
            "generated": generated_phase_report,
        }),
    );
    let report_update = snapshot_issue_reports(&cwd);
    if report_update.has_any() {
        issues::update_reports(
            pool,
            issue_id,
            report_update.agent_summary.as_deref(),
            report_update.progress_report.as_deref(),
            report_update.result_report.as_deref(),
            deps.clock.now_ms(),
        )
        .await?;
        append_system_event(
            &log_path,
            serde_json::json!({
                "kind": "issue_reports",
                "run_id": run_id,
                "issue_id": issue_id,
                "plan": report_update.agent_summary.is_some(),
                "progress": report_update.progress_report.is_some(),
                "human_verify": report_update.result_report.is_some(),
            }),
        );
    }
    append_system_event(
        &log_path,
        serde_json::json!({
            "kind": "finish",
            "run_id": run_id,
            "issue_id": issue_id,
            "result": "ok",
            "exit_kind": outcome.exit_kind.as_str(),
            "exit_code": outcome.exit_code,
            "status_after": status_after.clone(),
            "note": finish_note,
        }),
    );
    agent_runs::finish(
        pool,
        run_id,
        status_after.as_deref(),
        outcome.exit_code.map(|c| c as i64),
        outcome.exit_kind,
        deps.clock.now_ms(),
        finish_note.as_deref(),
    )
    .await?;

    Ok(())
}

#[derive(Debug, Default)]
struct IssueReportUpdate {
    agent_summary: Option<String>,
    progress_report: Option<String>,
    result_report: Option<String>,
}

impl IssueReportUpdate {
    fn has_any(&self) -> bool {
        self.agent_summary.is_some()
            || self.progress_report.is_some()
            || self.result_report.is_some()
    }
}

fn snapshot_issue_reports(cwd: &Path) -> IssueReportUpdate {
    let dir = cwd.join(".auwsx");
    IssueReportUpdate {
        agent_summary: read_issue_report(&dir.join("plan.md")),
        progress_report: read_issue_report(&dir.join("progress.md")),
        result_report: read_issue_report(&dir.join("human-verify.md")),
    }
}

fn read_phase_report(cwd: &Path) -> Option<String> {
    read_issue_report(&cwd.join(".auwsx").join(PHASE_REPORT_FILE))
}

fn fallback_phase_report(
    role: Role,
    phase: &str,
    exit_kind: &ExitKind,
    exit_code: Option<i32>,
    finish_note: Option<&str>,
) -> String {
    let mut report = format!(
        "Phase report missing: agent did not write .auwsx/{PHASE_REPORT_FILE}.\nrole={}\nphase={phase}\nexit_kind={}\nexit_code={}",
        role.as_str(),
        exit_kind.as_str(),
        exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "none".to_string())
    );
    if let Some(note) = finish_note {
        report.push_str("\nnote=");
        report.push_str(note);
    }
    report
}

fn read_issue_report(path: &Path) -> Option<String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("reading issue report {} failed: {e:#}", path.display());
            return None;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        tracing::warn!(
            "reading issue report {} skipped: not a regular file",
            path.display()
        );
        return None;
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("reading issue report {} failed: {e:#}", path.display());
            return None;
        }
    };
    let mut bytes = Vec::new();
    let mut limited = file.take((ISSUE_REPORT_LIMIT + 1) as u64);
    if let Err(e) = limited.read_to_end(&mut bytes) {
        tracing::warn!("reading issue report {} failed: {e:#}", path.display());
        return None;
    }
    let truncated = bytes.len() > ISSUE_REPORT_LIMIT;
    if truncated {
        bytes.truncate(ISSUE_REPORT_LIMIT);
        while !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
            bytes.pop();
        }
    }
    let mut text = String::from_utf8_lossy(&bytes).to_string();
    if text.trim().is_empty() {
        return None;
    }
    if truncated {
        text.push_str("\n[truncated]");
    }
    Some(text)
}

fn append_system_event(path: &std::path::Path, event: serde_json::Value) {
    if let Err(e) = artifacts::append_system_event(path, event) {
        tracing::warn!("writing issue system log {} failed: {e:#}", path.display());
    }
}

async fn mark_issue_failed(
    pool: &sqlx::SqlitePool,
    events: &broadcast::Sender<Event>,
    issue_id: i64,
    now: i64,
) -> Result<()> {
    if issues::transition(pool, issue_id, IssueStatus::Failed, now)
        .await
        .is_ok()
    {
        let _ = events.send(Event::IssueStatus {
            issue_id,
            status: IssueStatus::Failed,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn mark_issue_failed_with_setup_log(
    deps: &Deps<'_>,
    pool: &sqlx::SqlitePool,
    project: &projects::Project,
    issue_id: i64,
    role: Role,
    phase: &str,
    error: &anyhow::Error,
    now: i64,
) -> Result<()> {
    let note = format!("setup failed before agent spawn: {error:#}");
    if let Err(log_error) =
        record_setup_failure(deps, pool, project, issue_id, role, phase, &note, now).await
    {
        tracing::warn!("recording setup failure for issue {issue_id} failed: {log_error:#}");
    }
    mark_issue_failed(pool, deps.events, issue_id, now).await
}

#[allow(clippy::too_many_arguments)]
async fn record_setup_failure(
    deps: &Deps<'_>,
    pool: &sqlx::SqlitePool,
    project: &projects::Project,
    issue_id: i64,
    role: Role,
    phase: &str,
    note: &str,
    now: i64,
) -> Result<()> {
    let (log_path, _) = artifacts::issue_run_paths(issue_id, now)?;
    let log_str = log_path.to_string_lossy().to_string();
    let cmd_template = agent::expand_cmd_template(
        project.agent_cmd_for(role),
        agent::AgentTemplateVars::issue(&deps.socket, Path::new(".")),
    );
    let run_id = agent_runs::start(
        pool,
        StartRun {
            issue_id: Some(issue_id),
            main_job_id: None,
            role,
            phase,
            agent_cmd: &cmd_template,
            status_before: Some(phase),
            pid: None,
            prompt_path: None,
            log_path: Some(&log_str),
        },
        now,
    )
    .await?;
    append_system_event(
        &log_path,
        serde_json::json!({
            "kind": "setup_error",
            "run_id": run_id,
            "issue_id": issue_id,
            "role": role.as_str(),
            "phase": phase,
            "cmd": cmd_template,
            "note": note,
        }),
    );
    agent_runs::set_phase_report(pool, run_id, note).await?;
    agent_runs::finish(
        pool,
        run_id,
        Some(IssueStatus::Failed.as_str()),
        None,
        ExitKind::Error,
        now,
        Some(note),
    )
    .await?;
    append_system_event(
        &log_path,
        serde_json::json!({
            "kind": "finish",
            "run_id": run_id,
            "issue_id": issue_id,
            "result": "setup_error",
            "exit_kind": ExitKind::Error.as_str(),
            "exit_code": null,
            "status_after": IssueStatus::Failed.as_str(),
            "note": note,
        }),
    );
    Ok(())
}

#[cfg(test)]
mod report_tests {
    use super::*;

    #[test]
    fn given_issue_report_symlink_when_read_then_ignored() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let target = tmp.path().join("secret.txt");
        let link = tmp.path().join("phase-report.md");
        std::fs::write(&target, "do not snapshot")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link)?;

        #[cfg(unix)]
        assert!(read_issue_report(&link).is_none());
        Ok(())
    }

    #[test]
    fn given_issue_report_directory_when_read_then_ignored() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        assert!(read_issue_report(tmp.path()).is_none());
        Ok(())
    }

    #[test]
    fn given_oversized_issue_report_when_read_then_truncated() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let report = tmp.path().join("phase-report.md");
        std::fs::write(&report, "x".repeat(ISSUE_REPORT_LIMIT + 128))?;

        let got = read_issue_report(&report).expect("report is readable");

        assert!(got.ends_with("\n[truncated]"));
        assert!(got.len() <= ISSUE_REPORT_LIMIT + "\n[truncated]".len());
        Ok(())
    }

    #[test]
    fn given_missing_phase_report_when_fallback_built_then_explains_gap() {
        let got = fallback_phase_report(
            Role::Work,
            "WORKING",
            &ExitKind::Exited,
            Some(0),
            Some("agent exited without changing issue status; marked FAILED"),
        );

        assert!(got.contains("Phase report missing"));
        assert!(got.contains("role=work"));
        assert!(got.contains("phase=WORKING"));
        assert!(got.contains("exit_code=0"));
        assert!(got.contains("marked FAILED"));
    }
}

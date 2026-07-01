//! Main-job execution runtime.
//!
//! `main_jobs` owns the persisted queue rows. This module owns the imperative
//! execution path: create per-run artifacts, invoke the main agent, record
//! `agent_runs`, and land the main job in a terminal status.

use crate::agent::{self, AgentExecutor, AgentSpec, ExitKind};
use crate::artifacts;
use crate::clock::Clock;
use crate::db::agent_runs::{self, Role, StartRun};
use crate::db::projects::Project;
use crate::db::{projects, Db};
use crate::events::Event;
use crate::main_jobs::{self, MainJobStatus};
use crate::routines::{self, OutputRoute};
use crate::Result;
use anyhow::anyhow;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::broadcast;

pub struct Deps<'a> {
    pub db: &'a Db,
    pub clock: &'a dyn Clock,
    pub executor: &'a dyn AgentExecutor,
    pub events: &'a broadcast::Sender<Event>,
    pub socket: PathBuf,
}

pub struct RoutineJob {
    pub main_job_id: i64,
    pub routine_id: Option<i64>,
    pub output_route: OutputRoute,
    pub project: Project,
    pub prompt: String,
    pub phase: &'static str,
}

pub async fn enqueue_routine(deps: &Deps<'_>, routine_id: i64) -> Result<RoutineJob> {
    let pool = deps.db.pool();
    let routine = routines::get(pool, routine_id)
        .await?
        .ok_or_else(|| anyhow!("routine {routine_id} not found"))?;
    let project = projects::get(pool, routine.project_id)
        .await?
        .ok_or_else(|| anyhow!("routine {routine_id} references missing project"))?;
    let main_job_id = main_jobs::enqueue_routine(
        pool,
        project.id,
        routine.id,
        routine.output_route.as_str(),
        &routine.prompt,
        deps.clock.now_ms(),
    )
    .await?;
    let _ = deps.events.send(Event::RoutineFired {
        routine_id,
        main_job_id,
    });
    Ok(RoutineJob {
        main_job_id,
        routine_id: Some(routine_id),
        output_route: routine.output_route,
        project,
        prompt: routine.prompt,
        phase: "routine",
    })
}

pub async fn enqueue_memory_job(
    deps: &Deps<'_>,
    project_id: i64,
    kind: &'static str,
    issue_id: Option<i64>,
) -> Result<RoutineJob> {
    let pool = deps.db.pool();
    let project = projects::get(pool, project_id)
        .await?
        .ok_or_else(|| anyhow!("project {project_id} not found"))?;
    let main_job_id = match kind {
        "dream" => {
            let issue_id =
                issue_id.ok_or_else(|| anyhow!("post-merge dream requires an issue id"))?;
            main_jobs::enqueue_post_merge_dream(pool, project.id, issue_id, deps.clock.now_ms())
                .await?
        }
        "deepsleep" => {
            main_jobs::enqueue_project_deepsleep(pool, project.id, deps.clock.now_ms()).await?
        }
        other => return Err(anyhow!("unsupported memory main job kind {other:?}")),
    };
    Ok(RoutineJob {
        main_job_id,
        routine_id: None,
        output_route: OutputRoute::Memory,
        project,
        prompt: format!("Use auwsx memory consolidate --mode {kind}."),
        phase: kind,
    })
}

pub async fn execute_routine(deps: &Deps<'_>, job: &RoutineJob) -> Result<MainJobStatus> {
    let pool = deps.db.pool();
    let started_at = deps.clock.now_ms();
    let log_path = artifacts::main_job_log_path(job.project.id, job.main_job_id, started_at)?;
    let log_str = log_path.to_string_lossy().to_string();
    main_jobs::mark_running(pool, job.main_job_id, started_at, &log_str).await?;
    let _ = deps.events.send(Event::MainJobStatus {
        main_job_id: job.main_job_id,
        status: MainJobStatus::Running,
    });

    let env = main_job_env(&deps.socket, job);
    let cmd_template = agent::expand_cmd_template(
        &job.project.main_agent_cmd,
        agent::AgentTemplateVars::main_job(&deps.socket),
    );
    let run_id = agent_runs::start(
        pool,
        StartRun {
            issue_id: None,
            main_job_id: Some(job.main_job_id),
            role: Role::Main,
            phase: job.phase,
            agent_cmd: &cmd_template,
            status_before: Some(MainJobStatus::Queued.as_str()),
            pid: None,
            prompt_path: None,
            log_path: Some(&log_str),
        },
        started_at,
    )
    .await?;

    let timeout = Duration::from_secs((job.project.main_job_timeout_min.max(1) as u64) * 60);
    let cwd = PathBuf::from(&job.project.repo_path);
    let prompt = routine_prompt(job);
    let outcome = match deps
        .executor
        .execute(AgentSpec {
            cmd_template: &cmd_template,
            prompt: &prompt,
            cwd: &cwd,
            log_path: &log_path,
            timeout,
            env: &env,
        })
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            finish_failed(deps, run_id, job.main_job_id, e.to_string()).await?;
            return Err(e);
        }
    };
    if let Some(pid) = outcome.pid {
        agent_runs::set_pid(pool, run_id, pid as i64).await?;
    }

    let status = if outcome.exit_kind == ExitKind::Exited && outcome.exit_code == Some(0) {
        MainJobStatus::Done
    } else {
        MainJobStatus::Failed
    };
    let outcome_note = format!(
        "exit_kind={} exit_code={}",
        outcome.exit_kind.as_str(),
        outcome
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "none".to_string())
    );
    let ended_at = deps.clock.now_ms();
    main_jobs::finish(pool, job.main_job_id, status, ended_at, Some(&outcome_note)).await?;
    if let Some(routine_id) = job.routine_id {
        routines::mark_ran(pool, routine_id, ended_at).await?;
    }
    agent_runs::finish(
        pool,
        run_id,
        Some(status.as_str()),
        outcome.exit_code.map(|c| c as i64),
        outcome.exit_kind,
        ended_at,
        Some(&outcome_note),
    )
    .await?;
    Ok(status)
}

fn routine_prompt(job: &RoutineJob) -> String {
    let memory = crate::prompt::MemoryInvocation::from_agent_cmd(&job.project.main_agent_cmd)
        .skill("memory-consolidate");
    match job.output_route {
        OutputRoute::Report => format!(
            "ROUTE: report. Produce a concise report and do not edit project source files.\n\n{}",
            job.prompt
        ),
        OutputRoute::Backlog => format!(
            "ROUTE: backlog. Produce candidate backlog items only; auwsx will keep routine-authored backlog pending for approval.\n\n{}",
            job.prompt
        ),
        OutputRoute::Memory => format!(
            "ROUTE: memory. Use auwsx memory operations such as {memory}; do not edit project source files or bypass the issue pipeline.\n\n{}",
            job.prompt
        ),
    }
}

async fn finish_failed(deps: &Deps<'_>, run_id: i64, main_job_id: i64, note: String) -> Result<()> {
    let ended_at = deps.clock.now_ms();
    main_jobs::finish(
        deps.db.pool(),
        main_job_id,
        MainJobStatus::Failed,
        ended_at,
        Some(&note),
    )
    .await?;
    agent_runs::finish(
        deps.db.pool(),
        run_id,
        Some(MainJobStatus::Failed.as_str()),
        None,
        ExitKind::Error,
        ended_at,
        Some(&note),
    )
    .await
}

fn main_job_env(socket: &std::path::Path, job: &RoutineJob) -> Vec<(String, String)> {
    vec![
        (
            "AUWSX_SOCK".to_string(),
            socket.to_string_lossy().to_string(),
        ),
        (
            "AUWSX_BIN".to_string(),
            std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "auwsx".to_string()),
        ),
        ("AUWSX_PROJECT_ID".to_string(), job.project.id.to_string()),
        ("AUWSX_MAIN_JOB_ID".to_string(), job.main_job_id.to_string()),
        (
            "AUWSX_ROUTINE_ID".to_string(),
            job.routine_id.map(|id| id.to_string()).unwrap_or_default(),
        ),
        (
            "AUWSX_AGENT_ROLE".to_string(),
            Role::Main.as_str().to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::codex;

    fn project() -> Project {
        Project {
            id: 1,
            profile_id: 1,
            profile_order: 0,
            name: "demo".to_string(),
            repo_path: ".".to_string(),
            default_branch: "main".to_string(),
            arsenal_preset_name: None,
            main_agent_cmd: codex::DEFAULT_CMD.to_string(),
            route_agent_cmd: codex::DEFAULT_CMD.to_string(),
            plan_agent_cmd: codex::DEFAULT_CMD.to_string(),
            work_agent_cmd: codex::DEFAULT_CMD.to_string(),
            review_agent_cmd: None,
            main_agent_cmd_override: None,
            route_agent_cmd_override: None,
            plan_agent_cmd_override: None,
            work_agent_cmd_override: None,
            review_agent_cmd_override: None,
            skill_path: None,
            merge_mode: crate::db::projects::MergeMode::Local,
            completion_policy: crate::db::projects::CompletionPolicy::Manual,
            plan_gate_timeout_min: 10,
            completion_soft_timeout_min: 60,
            schedule_cron: None,
            max_concurrency: 3,
            iteration_timeout_min: 30,
            main_job_timeout_min: 60,
            review_max_rounds: 5,
            conflict_max_attempts: 3,
            deepsleep_cron: None,
            last_deepsleep_at: None,
            created_at: 1,
        }
    }

    fn job(output_route: OutputRoute) -> RoutineJob {
        RoutineJob {
            main_job_id: 1,
            routine_id: Some(1),
            output_route,
            project: project(),
            prompt: "summarize project state".to_string(),
            phase: "routine",
        }
    }

    #[test]
    fn given_report_route_when_prompt_built_then_source_edits_are_forbidden() {
        let prompt = routine_prompt(&job(OutputRoute::Report));

        assert!(prompt.contains("ROUTE: report"));
        assert!(prompt.contains("do not edit project source files"));
    }

    #[test]
    fn given_backlog_route_when_prompt_built_then_backlog_stays_pending() {
        let prompt = routine_prompt(&job(OutputRoute::Backlog));

        assert!(prompt.contains("ROUTE: backlog"));
        assert!(prompt.contains("candidate backlog items only"));
        assert!(prompt.contains("pending for approval"));
    }

    #[test]
    fn given_memory_route_when_prompt_built_then_memory_contract_is_explicit() {
        let prompt = routine_prompt(&job(OutputRoute::Memory));

        assert!(prompt.contains("ROUTE: memory"));
        assert!(prompt.contains("$memory-consolidate"));
        assert!(prompt.contains("do not edit project source files"));
        assert!(prompt.contains("do not"));
        assert!(prompt.contains("bypass the issue pipeline"));
    }
}

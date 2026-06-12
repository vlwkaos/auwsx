//! Main-job execution runtime.
//!
//! `main_jobs` owns the persisted queue rows. This module owns the imperative
//! execution path: create per-run artifacts, invoke the main agent, record
//! `agent_runs`, and land the main job in a terminal status.

use crate::agent::{AgentExecutor, AgentSpec, ExitKind};
use crate::artifacts;
use crate::clock::Clock;
use crate::db::agent_runs::{self, Role, StartRun};
use crate::db::projects::Project;
use crate::db::{projects, Db};
use crate::events::Event;
use crate::main_jobs::{self, MainJobStatus};
use crate::routines;
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
    pub routine_id: i64,
    pub project: Project,
    pub prompt: String,
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
        routine.routine_type.as_str(),
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
        routine_id,
        project,
        prompt: routine.prompt,
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
    let run_id = agent_runs::start(
        pool,
        StartRun {
            issue_id: None,
            main_job_id: Some(job.main_job_id),
            role: Role::Main,
            phase: "routine",
            agent_cmd: &job.project.main_agent_cmd,
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
    let outcome = match deps
        .executor
        .execute(AgentSpec {
            cmd_template: &job.project.main_agent_cmd,
            prompt: &job.prompt,
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
    routines::mark_ran(pool, job.routine_id, ended_at).await?;
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
        ("AUWSX_ROUTINE_ID".to_string(), job.routine_id.to_string()),
        (
            "AUWSX_AGENT_ROLE".to_string(),
            Role::Main.as_str().to_string(),
        ),
    ]
}

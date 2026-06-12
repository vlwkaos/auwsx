//! Per-project scheduler. Status is the sync marker: each tick reads issues and
//! acts by scheduler class — `Actionable` → spawn the phase agent (up to
//! `max_concurrency`, never double-spawning a running issue); `HumanGated` →
//! wait, except soft gates (`PLANNED`, and `ENDED` under soft/auto completion
//! policy) which arm a deadline and auto-release; `Terminal` → tear down a
//! finished issue's worktree.
//!
//! [`decide`] is pure (issues + project + running-set + now → decisions) and
//! fully testable; [`Scheduler`] is the runtime that executes decisions and
//! owns the in-flight set.

use crate::agent::AgentExecutor;
use crate::backlog;
use crate::clock::Clock;
use crate::db::issues;
use crate::db::projects::{self, CompletionPolicy, Project};
use crate::db::scheduler_runs::{
    self, SchedulerRunDecision, SchedulerRunPicked, SchedulerRunSource,
};
use crate::db::Issue;
use crate::events::Event;
use crate::main_job_runner;
use crate::main_jobs::MainJobStatus;
use crate::pipeline::{self, Deps};
use crate::state::{IssueStatus, SchedulerClass};
use crate::worktree::{branch_for_issue, WorktreeHandle, Worktrees};
use crate::Result;
use anyhow::{anyhow, bail};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;

/// One action a tick decides to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Spawn the phase agent for this actionable issue.
    Spawn(i64),
    /// Service a soft gate: arm its deadline if unset, or release it if expired.
    SoftGate(i64),
    /// Tear down a finished (DONE) issue's worktree.
    Teardown(i64),
}

/// Pure scheduling decision for one project's issues. `running` is the set of
/// issue ids with a live agent (must not be re-spawned); `now` is epoch ms.
///
/// Concurrency: at most `max_concurrency` issues run at once, so new spawns are
/// capped by the free slots after the already-running ones.
pub fn decide(
    issues: &[Issue],
    project: &Project,
    running: &HashSet<i64>,
    now: i64,
) -> Vec<Decision> {
    let mut out = Vec::new();
    let cap = project.max_concurrency.max(0) as usize;
    let mut slots = cap.saturating_sub(running.len());

    for issue in issues {
        match issue.status.scheduler_class() {
            SchedulerClass::Actionable => {
                if running.contains(&issue.id) {
                    continue; // its agent is still alive
                }
                if slots > 0 {
                    out.push(Decision::Spawn(issue.id));
                    slots -= 1;
                }
            }
            SchedulerClass::HumanGated => {
                if soft_gate_due(issue, project, now) {
                    out.push(Decision::SoftGate(issue.id));
                }
            }
            SchedulerClass::Terminal => {
                if issue.status == IssueStatus::Done && issue.worktree_path.is_some() {
                    out.push(Decision::Teardown(issue.id));
                }
            }
        }
    }
    out
}

/// Whether a human-gated issue is on a soft (auto-releasing) gate at all.
/// `PLANNED` always is; `ENDED` is under `soft`/`auto` completion policy; the
/// `*_BLOCKED` gates never are (they wait for an explicit human).
fn soft_releasable(issue: &Issue, project: &Project) -> bool {
    match issue.status {
        IssueStatus::Planned => true,
        IssueStatus::Ended => matches!(
            project.completion_policy,
            CompletionPolicy::Soft | CompletionPolicy::Auto
        ),
        _ => false,
    }
}

/// Whether a soft gate needs servicing this tick: it must be soft-releasable AND
/// either not yet armed (`wait_until` unset) or past its deadline. An armed gate
/// that is not yet due produces no decision (no churn).
fn soft_gate_due(issue: &Issue, project: &Project, now: i64) -> bool {
    if !soft_releasable(issue, project) {
        return false;
    }
    match issue.wait_until {
        None => true,
        Some(w) => now >= w,
    }
}

/// The autonomous runtime around [`decide`] + [`pipeline::execute`].
#[derive(Clone)]
pub struct Scheduler {
    db: crate::db::Db,
    clock: Arc<dyn Clock>,
    executor: Arc<dyn AgentExecutor>,
    worktrees: Arc<dyn Worktrees>,
    events: broadcast::Sender<Event>,
    socket: PathBuf,
    tick_interval: Duration,
    running: Arc<Mutex<HashSet<i64>>>,
    running_routines: Arc<Mutex<HashSet<i64>>>,
    inflight: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Scheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: crate::db::Db,
        clock: Arc<dyn Clock>,
        executor: Arc<dyn AgentExecutor>,
        worktrees: Arc<dyn Worktrees>,
        events: broadcast::Sender<Event>,
        socket: PathBuf,
        tick_interval: Duration,
    ) -> Self {
        Scheduler {
            db,
            clock,
            executor,
            worktrees,
            events,
            socket,
            tick_interval,
            running: Arc::new(Mutex::new(HashSet::new())),
            running_routines: Arc::new(Mutex::new(HashSet::new())),
            inflight: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Run until `shutdown` fires: every `tick_interval`, tick all projects.
    pub async fn run(&self, shutdown: Arc<Notify>) {
        let mut interval = tokio::time::interval(self.tick_interval);
        loop {
            tokio::select! {
                _ = shutdown.notified() => break,
                _ = interval.tick() => {
                    let projects = projects::list(self.db.pool()).await.unwrap_or_default();
                    let now = self.clock.now_ms();
                    for p in projects {
                        if !self.project_due_for_auto_tick(&p, now).await {
                            continue;
                        }
                        if let Err(e) = self.tick_project_from(p.id, SchedulerRunSource::Auto).await {
                            tracing::warn!("scheduler tick for project {} failed: {e:#}", p.id);
                        }
                    }
                }
            }
        }
    }

    /// Whether the automatic daemon loop should run this project on the current
    /// global tick. Manual commands intentionally bypass this gate by calling
    /// `tick_project` directly.
    async fn project_due_for_auto_tick(&self, project: &Project, now: i64) -> bool {
        let Some(interval_min) = project.schedule_interval_min else {
            return false;
        };
        if interval_min <= 0 {
            return true;
        }

        let interval_ms = interval_min.saturating_mul(60_000);
        match scheduler_runs::latest_auto_by_project(self.db.pool(), project.id).await {
            Ok(Some(last)) => now.saturating_sub(last.fired_at) >= interval_ms,
            Ok(None) => true,
            Err(e) => {
                tracing::warn!(
                    "checking scheduler interval for project {} failed: {e:#}",
                    project.id
                );
                true
            }
        }
    }

    /// One scheduling pass for a project: decide, then execute each decision.
    pub async fn tick_project(&self, project_id: i64) -> Result<()> {
        self.tick_project_from(project_id, SchedulerRunSource::Manual)
            .await
    }

    async fn tick_project_from(&self, project_id: i64, source: SchedulerRunSource) -> Result<()> {
        self.prune_inflight();
        let pool = self.db.pool();
        let Some(project) = projects::get(pool, project_id).await? else {
            return Ok(());
        };
        let snapshot = self.running.lock().unwrap().clone();
        let now = self.clock.now_ms();
        let triaged = match backlog::run_triage_detailed(pool, project_id, now).await {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!("triage for project {project_id} failed: {e:#}");
                Vec::new()
            }
        };
        let issues = issues::list_by_project(pool, project_id).await?;
        let issue_ids: HashSet<i64> = issues.iter().map(|issue| issue.id).collect();
        let project_running: HashSet<i64> = snapshot
            .iter()
            .copied()
            .filter(|issue_id| issue_ids.contains(issue_id))
            .collect();
        let decisions = decide(&issues, &project, &project_running, now);
        let triaged_issue_ids: Vec<i64> = triaged.iter().map(|item| item.issue_id).collect();
        let pending_backlog =
            backlog::count_by_approval(pool, project_id, backlog::Approval::Pending)
                .await
                .unwrap_or(0) as usize;
        let ready_backlog =
            backlog::count_unconsumed_by_approval(pool, project_id, backlog::Approval::Approved)
                .await
                .unwrap_or(0) as usize;
        let picked = picked_summary(
            &triaged_issue_ids,
            &decisions,
            pending_backlog,
            ready_backlog,
            project_running.len(),
            project.max_concurrency.max(0) as usize,
        );
        match picked.to_json_string() {
            Ok(picked_json) => {
                if let Err(e) =
                    scheduler_runs::record(pool, project_id, now, source, Some(&picked_json)).await
                {
                    tracing::warn!(
                        "recording scheduler tick for project {project_id} failed: {e:#}"
                    );
                }
            }
            Err(e) => {
                tracing::warn!("serializing scheduler tick for project {project_id} failed: {e:#}")
            }
        }
        let _ = self.events.send(Event::SchedulerTick { project_id });
        for item in &triaged {
            let _ = self.events.send(Event::BacklogChanged {
                item_id: item.item_id,
                project_id,
                approval: "approved".to_string(),
            });
            let _ = self.events.send(Event::IssueStatus {
                issue_id: item.issue_id,
                status: IssueStatus::Consolidating,
            });
        }

        for d in decisions {
            match d {
                Decision::Spawn(id) => self.spawn_phase(id),
                Decision::SoftGate(id) => {
                    if let Err(e) = self.service_soft_gate(&project, id).await {
                        tracing::warn!("soft-gate for issue {id} failed: {e:#}");
                    }
                }
                Decision::Teardown(id) => {
                    if let Err(e) = self.teardown(&project, id).await {
                        tracing::warn!("worktree teardown for issue {id} failed: {e:#}");
                    }
                }
            }
        }
        Ok(())
    }

    /// Execute the selected issue immediately if it is actionable and not
    /// already running. This is the daemon-owned imperative control used by
    /// clients for "run now"; it shares the same running set and pipeline as the
    /// automatic tick.
    pub async fn run_issue_now(&self, issue_id: i64) -> Result<()> {
        self.prune_inflight();
        if self.running.lock().unwrap().contains(&issue_id) {
            bail!("issue {issue_id} is already running");
        }
        let issue = issues::get(self.db.pool(), issue_id)
            .await?
            .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
        if pipeline::plan_phase(issue.status).is_none() {
            bail!(
                "issue {issue_id} is not actionable in status {}",
                issue.status.as_str()
            );
        }
        self.spawn_phase(issue_id);
        Ok(())
    }

    pub async fn run_backlog_now(&self, item_id: i64, now: i64) -> Result<i64> {
        let issue_id = backlog::promote_one(self.db.pool(), item_id, now).await?;
        self.run_issue_now(issue_id).await?;
        Ok(issue_id)
    }

    pub fn is_running(&self, issue_id: i64) -> bool {
        self.running.lock().unwrap().contains(&issue_id)
    }

    /// Enqueue and run a routine immediately through the main-job lane.
    pub async fn run_routine_now(&self, routine_id: i64) -> Result<i64> {
        self.prune_inflight();
        {
            let mut running = self.running_routines.lock().unwrap();
            if running.contains(&routine_id) {
                bail!("routine {routine_id} is already running");
            }
            running.insert(routine_id);
        }

        let result = self.enqueue_and_spawn_routine(routine_id).await;
        if result.is_err() {
            self.running_routines.lock().unwrap().remove(&routine_id);
        }
        result
    }

    async fn enqueue_and_spawn_routine(&self, routine_id: i64) -> Result<i64> {
        let deps = main_job_runner::Deps {
            db: &self.db,
            clock: &*self.clock,
            executor: &*self.executor,
            events: &self.events,
            socket: self.socket.clone(),
        };
        let job = main_job_runner::enqueue_routine(&deps, routine_id).await?;
        let main_job_id = job.main_job_id;
        self.spawn_main_job(job);
        Ok(main_job_id)
    }

    fn spawn_main_job(&self, job: main_job_runner::RoutineJob) {
        let db = self.db.clone();
        let clock = self.clock.clone();
        let executor = self.executor.clone();
        let events = self.events.clone();
        let socket = self.socket.clone();
        let running_routines = self.running_routines.clone();
        let main_job_id = job.main_job_id;
        let routine_id = job.routine_id;

        let handle = tokio::spawn(async move {
            let deps = main_job_runner::Deps {
                db: &db,
                clock: &*clock,
                executor: &*executor,
                events: &events,
                socket,
            };
            let status = match main_job_runner::execute_routine(&deps, &job).await {
                Ok(status) => status,
                Err(e) => {
                    tracing::warn!("main job {main_job_id} failed: {e:#}");
                    MainJobStatus::Failed
                }
            };
            let _ = events.send(Event::MainJobStatus {
                main_job_id,
                status,
            });
            running_routines.lock().unwrap().remove(&routine_id);
        });
        self.inflight.lock().unwrap().push(handle);
    }

    /// Reserve the slot and spawn the phase task; it removes itself from the
    /// running set when it finishes.
    fn spawn_phase(&self, issue_id: i64) {
        {
            let mut running = self.running.lock().unwrap();
            if running.contains(&issue_id) {
                return;
            }
            running.insert(issue_id);
        }
        let db = self.db.clone();
        let clock = self.clock.clone();
        let executor = self.executor.clone();
        let worktrees = self.worktrees.clone();
        let events = self.events.clone();
        let socket = self.socket.clone();
        let running = self.running.clone();

        let handle = tokio::spawn(async move {
            let deps = Deps {
                db: &db,
                clock: &*clock,
                executor: &*executor,
                worktrees: &*worktrees,
                events: &events,
                socket,
            };
            if let Err(e) = pipeline::execute(&deps, issue_id).await {
                tracing::warn!("phase execution for issue {issue_id} failed: {e:#}");
            }
            running.lock().unwrap().remove(&issue_id);
        });
        self.inflight.lock().unwrap().push(handle);
    }

    /// Arm a soft gate (set `wait_until`) or release it (transition) when due.
    async fn service_soft_gate(&self, project: &Project, issue_id: i64) -> Result<()> {
        let pool = self.db.pool();
        let Some(issue) = issues::get(pool, issue_id).await? else {
            return Ok(());
        };
        let now = self.clock.now_ms();
        let (target, deadline_min) = match issue.status {
            IssueStatus::Planned => (IssueStatus::Implementing, project.plan_gate_timeout_min),
            IssueStatus::Ended => {
                let mins = match project.completion_policy {
                    CompletionPolicy::Auto => 0,
                    CompletionPolicy::Soft => project.completion_soft_timeout_min,
                    CompletionPolicy::Manual => return Ok(()), // not soft; decide filters this
                };
                (IssueStatus::Completing, mins)
            }
            _ => return Ok(()),
        };

        if deadline_min <= 0 {
            return self.release(issue_id, target, now).await; // auto: release now
        }
        match issue.wait_until {
            None => {
                issues::set_wait_until(pool, issue_id, Some(now + deadline_min * 60_000), now).await
            }
            Some(w) if now >= w => self.release(issue_id, target, now).await,
            Some(_) => Ok(()), // still waiting
        }
    }

    async fn release(&self, issue_id: i64, target: IssueStatus, now: i64) -> Result<()> {
        issues::transition(self.db.pool(), issue_id, target, now).await?;
        let _ = self.events.send(Event::IssueStatus {
            issue_id,
            status: target,
        });
        Ok(())
    }

    async fn teardown(&self, project: &Project, issue_id: i64) -> Result<()> {
        let pool = self.db.pool();
        let Some(issue) = issues::get(pool, issue_id).await? else {
            return Ok(());
        };
        let Some(path) = issue.worktree_path.clone() else {
            return Ok(());
        };
        let handle = WorktreeHandle {
            branch: issue
                .branch
                .clone()
                .unwrap_or_else(|| branch_for_issue(issue_id)),
            path: PathBuf::from(path),
        };
        self.worktrees.teardown(project, &handle).await?;
        // Clear the worktree fields so we don't try again next tick.
        issues::set_worktree(pool, issue_id, None, None, None, self.clock.now_ms()).await
    }

    /// Drop handles for tasks that have finished (keeps the in-flight Vec from
    /// growing without bound in the long-running daemon).
    fn prune_inflight(&self) {
        self.inflight.lock().unwrap().retain(|h| !h.is_finished());
    }

    /// Await all currently in-flight phase tasks. For tests + graceful drain.
    pub async fn join_inflight(&self) {
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *self.inflight.lock().unwrap());
        for h in handles {
            let _ = h.await;
        }
    }
}

fn picked_summary(
    triaged_issue_ids: &[i64],
    decisions: &[Decision],
    pending_backlog: usize,
    ready_backlog: usize,
    running_issues: usize,
    max_concurrency: usize,
) -> SchedulerRunPicked {
    let decisions = decisions
        .iter()
        .map(|d| match d {
            Decision::Spawn(issue_id) => SchedulerRunDecision::Spawn {
                issue_id: *issue_id,
            },
            Decision::SoftGate(issue_id) => SchedulerRunDecision::SoftGate {
                issue_id: *issue_id,
            },
            Decision::Teardown(issue_id) => SchedulerRunDecision::Teardown {
                issue_id: *issue_id,
            },
        })
        .collect();
    SchedulerRunPicked {
        triaged_issue_ids: triaged_issue_ids.to_vec(),
        decisions,
        pending_backlog,
        ready_backlog,
        running_issues,
        max_concurrency,
    }
}

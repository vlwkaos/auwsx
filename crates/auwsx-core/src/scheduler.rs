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
use crate::clock::Clock;
use crate::db::issues;
use crate::db::projects::{self, CompletionPolicy, Project};
use crate::db::Issue;
use crate::events::Event;
use crate::pipeline::{self, Deps};
use crate::state::{IssueStatus, SchedulerClass};
use crate::worktree::{branch_for_issue, WorktreeHandle, Worktrees};
use crate::Result;
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
                    for p in projects {
                        if let Err(e) = self.tick_project(p.id).await {
                            tracing::warn!("scheduler tick for project {} failed: {e:#}", p.id);
                        }
                    }
                }
            }
        }
    }

    /// One scheduling pass for a project: decide, then execute each decision.
    pub async fn tick_project(&self, project_id: i64) -> Result<()> {
        self.prune_inflight();
        let pool = self.db.pool();
        let Some(project) = projects::get(pool, project_id).await? else {
            return Ok(());
        };
        let issues = issues::list_by_project(pool, project_id).await?;
        let snapshot = self.running.lock().unwrap().clone();
        let decisions = decide(&issues, &project, &snapshot, self.clock.now_ms());

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
            branch: issue.branch.clone().unwrap_or_else(|| branch_for_issue(issue_id)),
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

//! Per-project scheduler. Status is the sync marker: each tick reads issues and
//! acts by scheduler class — `Actionable` → spawn the phase agent (up to
//! `max_concurrency`, never double-spawning a running issue); `HumanGated` →
//! wait, except soft gates (`PLAN_READY`, and `READY_TO_MERGE` under soft/auto completion
//! policy) which arm a deadline and auto-release; `Terminal` → tear down a
//! finished issue's worktree.
//!
//! [`decide`] is pure (issues + project + running-set + now → decisions) and
//! fully testable; [`Scheduler`] is the runtime that executes decisions and
//! owns the in-flight set.

use crate::agent::{self, AgentExecutor, AgentSpec, ExitKind};
use crate::artifacts;
use crate::backlog;
use crate::clock::Clock;
use crate::db::agent_runs;
use crate::db::ask_answers::{self, AskMode};
use crate::db::issues;
use crate::db::projects::{self, CompletionPolicy, MergeMode, Project};
use crate::db::scheduler_runs::{
    self, SchedulerRunDecision, SchedulerRunPicked, SchedulerRunRoute, SchedulerRunSource,
};
use crate::db::Issue;
use crate::events::Event;
use crate::issue_control::{self, ControlOutcome, IssueExecutePlan, ProjectExecutePlan};
use crate::main_job_runner;
use crate::main_jobs::{self, MainJobStatus};
use crate::pipeline::{self, Deps};
use crate::reconcile::{self, AgentReconcileAction, ProjectReconcileReport, ReconcileActionKind};
use crate::routines::{self, Routine};
use crate::routing;
use crate::state::{IssueStatus, SchedulerClass};
use crate::worktree::{
    branch_for_issue, prune_orphaned_issue_worktrees, WorktreeHandle, Worktrees,
};
use crate::Result;
use anyhow::{anyhow, bail, Context};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
    let local_merge_mode = project.merge_mode == MergeMode::Local;
    let local_merge_blocked = local_merge_mode
        && issues
            .iter()
            .any(|issue| issue.status == IssueStatus::ConflictBlocked);
    let mut local_merge_slot_taken = local_merge_mode
        && issues
            .iter()
            .any(|issue| issue.status == IssueStatus::Merging && running.contains(&issue.id));
    let mut ordered: Vec<(usize, &Issue)> = issues.iter().enumerate().collect();
    if local_merge_mode {
        ordered.sort_by_key(|(idx, issue)| {
            if issue.status == IssueStatus::Merging {
                (0usize, issue.id as usize)
            } else {
                (1usize, *idx)
            }
        });
    }

    for (_, issue) in ordered {
        match issue.status.scheduler_class() {
            SchedulerClass::Actionable => {
                if running.contains(&issue.id) {
                    continue; // its agent is still alive
                }
                if slots > 0 {
                    if local_merge_mode && issue.status == IssueStatus::Merging {
                        if local_merge_blocked {
                            continue;
                        }
                        if local_merge_slot_taken {
                            continue;
                        }
                        local_merge_slot_taken = true;
                    }
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
                if matches!(issue.status, IssueStatus::Done | IssueStatus::Abandoned)
                    && issue.worktree_path.is_some()
                {
                    out.push(Decision::Teardown(issue.id));
                }
            }
        }
    }
    out
}

/// Whether a human-gated issue is on a soft (auto-releasing) gate at all.
/// `PLAN_READY` always is; `READY_TO_MERGE` is under `soft`/`auto` completion policy; the
/// `*_BLOCKED` gates never are (they wait for an explicit human).
fn soft_releasable(issue: &Issue, project: &Project) -> bool {
    match issue.status {
        IssueStatus::PlanReady => true,
        IssueStatus::ReadyToMerge => matches!(
            project.completion_policy,
            CompletionPolicy::Soft | CompletionPolicy::Auto
        ),
        _ => false,
    }
}

fn ask_context_summary(
    project: &Project,
    issues: &[Issue],
    backlog: &[backlog::BacklogItem],
    routines: &[Routine],
) -> String {
    let mut lines = vec![
        format!("Project: {} ({})", project.name, project.repo_path),
        format!(
            "Policy: completion={} schedule={}",
            project.completion_policy.as_str(),
            crate::schedule::cadence_label(
                project.schedule_cron.as_deref(),
                project.schedule_interval_min
            )
        ),
        format!(
            "Counts: {} issues, {} live backlog, {} routines",
            issues.len(),
            backlog.len(),
            routines.len()
        ),
        "Issues:".to_string(),
    ];
    for issue in issues.iter().take(12) {
        lines.push(format!(
            "- #{} [{} / {}] {}",
            issue.id,
            issue.status.stage_label(),
            issue.status.as_str(),
            issue.title.lines().next().unwrap_or("")
        ));
    }
    if issues.is_empty() {
        lines.push("- none".to_string());
    }
    lines.push("Live backlog:".to_string());
    for item in backlog.iter().take(8) {
        lines.push(format!(
            "- #{} [{}] {}",
            item.id,
            item.approval.as_str(),
            item.text.lines().next().unwrap_or("")
        ));
    }
    if backlog.is_empty() {
        lines.push("- none".to_string());
    }
    lines.join("\n")
}

fn ask_prompt(mode: AskMode, question: &str, context: &str) -> String {
    let skill = match mode {
        AskMode::Recall => "$recall",
        AskMode::Seek => "$seek",
    };
    format!(
        "\
You are answering one operator question about the current auwsx project.

Use {skill} before answering. Use project status below as immediate context.
Do not modify files, do not run status-changing auwsx commands, and do not make commits.
Return only the final answer for the operator.

## Current Project Status
{context}

## Question
{question}
"
    )
}

fn reconcile_agent_prompt(report: &ProjectReconcileReport) -> Result<String> {
    let json = serde_json::to_string_pretty(report)?;
    Ok(format!(
        "\
ROUTE: report. You are auwsx's queued reconcile advisor.

Review the deterministic reconcile report below and propose only safe, minimal recovery actions.
Do not edit source files unless the report proves a manual conflict-resolution path is required.
Output JSON with keys: proposal, rationale, actions, verification, risk.
Wrap the final proposal in one final ```json fenced block and put no text after it.
The JSON object must include schema_version: 1 and kind: \"auwsx_reconcile_proposal\".
Allowed action names: mark_done, cleanup_worktree, retry_issue, apply_merge, manual_required.
The daemon will reject any action that does not pass deterministic validation at apply time.

## Deterministic Reconcile Report
{json}
"
    ))
}

fn extract_answer_from_log(text: &str) -> Option<String> {
    let mut last_agent_message = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(item) = value.get("item") else {
            continue;
        };
        if item.get("type").and_then(|v| v.as_str()) == Some("agent_message") {
            if let Some(msg) = item.get("text").and_then(|v| v.as_str()) {
                last_agent_message = Some(msg.to_string());
            }
        }
    }
    last_agent_message
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
    running_projects: Arc<Mutex<HashSet<i64>>>,
    running_routines: Arc<Mutex<HashSet<i64>>>,
    inflight: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

struct ProjectTickGuard {
    project_id: i64,
    running_projects: Arc<Mutex<HashSet<i64>>>,
}

impl Drop for ProjectTickGuard {
    fn drop(&mut self) {
        self.running_projects
            .lock()
            .unwrap()
            .remove(&self.project_id);
    }
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
            running_projects: Arc::new(Mutex::new(HashSet::new())),
            running_routines: Arc::new(Mutex::new(HashSet::new())),
            inflight: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn try_project_tick(&self, project_id: i64) -> Option<ProjectTickGuard> {
        let mut running_projects = self.running_projects.lock().unwrap();
        if !running_projects.insert(project_id) {
            return None;
        }
        Some(ProjectTickGuard {
            project_id,
            running_projects: Arc::clone(&self.running_projects),
        })
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
        match scheduler_runs::latest_auto_by_project(self.db.pool(), project.id).await {
            Ok(last) => match crate::schedule::is_due(
                project.schedule_cron.as_deref(),
                project.schedule_interval_min,
                last.map(|run| run.fired_at),
                project.created_at,
                now,
                self.tick_interval.as_secs() as i64,
            ) {
                Ok(due) => due,
                Err(e) => {
                    tracing::warn!(
                        "checking scheduler cron for project {} failed: {e:#}",
                        project.id
                    );
                    false
                }
            },
            Err(e) => {
                tracing::warn!(
                    "checking latest scheduler tick for project {} failed: {e:#}",
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
        let Some(_tick_guard) = self.try_project_tick(project_id) else {
            return Ok(());
        };
        self.prune_inflight();
        let pool = self.db.pool();
        let Some(project) = projects::get(pool, project_id).await? else {
            return Ok(());
        };
        let snapshot = self.running.lock().unwrap().clone();
        let now = self.clock.now_ms();
        let routed = match routing::route_approved_project_semantic(&routing::RouteDeps {
            pool,
            project: &project,
            executor: self.executor.as_ref(),
            socket: &self.socket,
            now,
        })
        .await
        {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!("routing backlog for project {project_id} failed: {e:#}");
                Vec::new()
            }
        };
        let route_outcomes: Vec<_> = routed.iter().map(|item| item.outcome.clone()).collect();
        let issues = issues::list_by_project(pool, project_id).await?;
        let issue_ids: HashSet<i64> = issues.iter().map(|issue| issue.id).collect();
        let project_running: HashSet<i64> = snapshot
            .iter()
            .copied()
            .filter(|issue_id| issue_ids.contains(issue_id))
            .collect();
        let decisions = decide(&issues, &project, &project_running, now);
        let triaged_issue_ids: Vec<i64> =
            route_outcomes.iter().map(|item| item.issue_id()).collect();
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
            &routed,
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
        for item in &route_outcomes {
            let _ = self.events.send(Event::BacklogChanged {
                item_id: item.item_id(),
                project_id,
                approval: "approved".to_string(),
            });
            if let Ok(Some(issue)) = issues::get(pool, item.issue_id()).await {
                let _ = self.events.send(Event::IssueStatus {
                    issue_id: item.issue_id(),
                    status: issue.status,
                });
            }
            if let routing::RouteOutcome::AttachedToIssue {
                issue_id,
                message_id,
                ..
            } = item
            {
                let _ = self.events.send(Event::SteeringAdded {
                    steering_id: *message_id,
                    issue_id: *issue_id,
                });
            }
        }

        if matches!(source, SchedulerRunSource::Auto) {
            if let Err(e) = self.service_project_deepsleep(&project, now).await {
                tracing::warn!("deepsleep scheduling for project {project_id} failed: {e:#}");
            }
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

    async fn service_project_deepsleep(&self, project: &Project, now: i64) -> Result<()> {
        let deepsleep_cron = project
            .deepsleep_cron
            .clone()
            .or_else(|| crate::schedule::legacy_deepsleep_to_cron(project.deepsleep_interval_days));
        let Some(deepsleep_cron) = deepsleep_cron else {
            return Ok(());
        };
        let due = match project.last_deepsleep_at {
            None => true,
            Some(last) => crate::schedule::is_due(
                Some(&deepsleep_cron),
                None,
                Some(last),
                project.created_at,
                now,
                self.tick_interval.as_secs() as i64,
            )?,
        };
        if !due {
            return Ok(());
        }
        if main_jobs::has_active_project_job(self.db.pool(), project.id).await? {
            return Ok(());
        }
        self.spawn_memory_job(project.id, "deepsleep", None);
        Ok(())
    }

    /// Execute the selected issue immediately if it is actionable and not
    /// already running. This is the daemon-owned imperative control used by
    /// clients for "run now"; it shares the same running set and pipeline as the
    /// automatic tick.
    pub async fn run_issue_now(&self, issue_id: i64) -> Result<()> {
        self.prune_inflight();
        self.ensure_issue_idle(issue_id)?;
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

    pub async fn execute_issue(&self, issue_id: i64, now: i64) -> Result<ControlOutcome> {
        self.prune_inflight();
        self.ensure_issue_idle(issue_id)?;
        let issue = issues::get(self.db.pool(), issue_id)
            .await?
            .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
        let runs = if issue.status == IssueStatus::Failed {
            agent_runs::list_by_issue(self.db.pool(), issue_id).await?
        } else {
            Vec::new()
        };

        match issue_control::plan_issue_execute(&issue, &runs)? {
            IssueExecutePlan::RunPhase => {
                self.spawn_phase(issue_id);
                Ok(ControlOutcome::RanIssue { issue_id })
            }
            IssueExecutePlan::RetryFailed { retry_status } => {
                self.retry_failed_issue_to_status(issue_id, retry_status, now)
                    .await?;
                Ok(ControlOutcome::RanIssue { issue_id })
            }
            IssueExecutePlan::ApproveMerge => {
                self.release(issue_id, IssueStatus::Merging, now).await?;
                self.tick_project(issue.project_id).await?;
                Ok(ControlOutcome::ApprovedMerge {
                    issue_ids: vec![issue_id],
                })
            }
        }
    }

    pub async fn execute_project(&self, project_id: i64, now: i64) -> Result<ControlOutcome> {
        projects::get(self.db.pool(), project_id)
            .await?
            .ok_or_else(|| anyhow!("project {project_id} not found"))?;
        let ready =
            issues::list_by_status(self.db.pool(), project_id, IssueStatus::ReadyToMerge).await?;

        match issue_control::plan_project_execute(ready.len()) {
            ProjectExecutePlan::TickScheduler => {
                self.tick_project(project_id).await?;
                Ok(ControlOutcome::Ok)
            }
            ProjectExecutePlan::ApproveReadyMergeQueue => {
                let issue_ids = self.approve_project_merge(project_id, now).await?;
                Ok(ControlOutcome::ApprovedMerge { issue_ids })
            }
        }
    }

    pub async fn retry_failed_issue(&self, issue_id: i64, now: i64) -> Result<()> {
        self.prune_inflight();
        self.ensure_issue_idle(issue_id)?;
        let issue = issues::get(self.db.pool(), issue_id)
            .await?
            .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
        if issue.status != IssueStatus::Failed {
            bail!(
                "issue {issue_id} is not failed; current status is {}",
                issue.status.as_str()
            );
        }

        let retry_status = self.retry_status_for_issue(issue_id).await?;
        self.retry_failed_issue_to_status(issue_id, retry_status, now)
            .await
    }

    async fn retry_failed_issue_to_status(
        &self,
        issue_id: i64,
        retry_status: IssueStatus,
        now: i64,
    ) -> Result<()> {
        issues::force_status(self.db.pool(), issue_id, retry_status, now).await?;
        let _ = self.events.send(Event::IssueStatus {
            issue_id,
            status: retry_status,
        });
        self.run_issue_now(issue_id).await
    }

    async fn retry_status_for_issue(&self, issue_id: i64) -> Result<IssueStatus> {
        let runs = agent_runs::list_by_issue(self.db.pool(), issue_id).await?;
        Ok(issue_control::retry_status_from_runs(&runs))
    }

    pub async fn approve_issue_merge(&self, issue_id: i64, now: i64) -> Result<Vec<i64>> {
        self.prune_inflight();
        self.ensure_issue_idle(issue_id)?;
        let issue = issues::get(self.db.pool(), issue_id)
            .await?
            .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
        self.ensure_local_merge_not_conflict_blocked(issue.project_id)
            .await?;
        if issue.status != IssueStatus::ReadyToMerge {
            bail!(
                "issue {issue_id} is not ready to merge; current status is {}",
                issue.status.as_str()
            );
        }
        self.release(issue_id, IssueStatus::Merging, now).await?;
        self.tick_project(issue.project_id).await?;
        Ok(vec![issue_id])
    }

    pub async fn approve_project_merge(&self, project_id: i64, now: i64) -> Result<Vec<i64>> {
        self.prune_inflight();
        projects::get(self.db.pool(), project_id)
            .await?
            .ok_or_else(|| anyhow!("project {project_id} not found"))?;
        self.ensure_local_merge_not_conflict_blocked(project_id)
            .await?;
        let preflight = self.diagnose_project(project_id, true).await?;
        if let Some(done_elsewhere) = preflight.issues.iter().find(|issue| {
            issue.status == IssueStatus::ReadyToMerge
                && issue.proposed_action == ReconcileActionKind::MarkDone
        }) {
            bail!(
                "project {project_id} issue {} is already represented in main; run `auwsx project reconcile {project_id}` before project merge",
                done_elsewhere.issue_id
            );
        }
        let blocker = preflight.issues.iter().find(|issue| {
            issue.status == IssueStatus::ReadyToMerge
                && issue.proposed_action != ReconcileActionKind::ApplyMerge
        });
        if let Some(blocker) = blocker {
            bail!(
                "project {project_id} has reconcile blocker on issue {}: {} ({})",
                blocker.issue_id,
                blocker.diagnosis.as_str(),
                blocker.blocking_reason.as_deref().unwrap_or("no detail")
            );
        }

        let ready =
            issues::list_by_status(self.db.pool(), project_id, IssueStatus::ReadyToMerge).await?;
        if ready.is_empty() {
            bail!("project {project_id} has no READY_TO_MERGE issues");
        }

        let mut released = Vec::with_capacity(ready.len());
        for issue in ready {
            if self.running.lock().unwrap().contains(&issue.id) {
                continue;
            }
            self.release(issue.id, IssueStatus::Merging, now).await?;
            released.push(issue.id);
        }

        if released.is_empty() {
            bail!("project {project_id} has no releasable READY_TO_MERGE issues");
        }

        self.tick_project(project_id).await?;
        Ok(released)
    }

    async fn ensure_local_merge_not_conflict_blocked(&self, project_id: i64) -> Result<()> {
        let Some(project) = projects::get(self.db.pool(), project_id).await? else {
            bail!("project {project_id} not found");
        };
        if project.merge_mode != MergeMode::Local {
            return Ok(());
        }
        let blocked =
            issues::list_by_status(self.db.pool(), project_id, IssueStatus::ConflictBlocked)
                .await?;
        if let Some(issue) = blocked.first() {
            bail!(
                "project {project_id} has conflict-blocked issue {}; resolve it before releasing more local merges",
                issue.id
            );
        }
        Ok(())
    }

    pub async fn diagnose_project(
        &self,
        project_id: i64,
        dry_run: bool,
    ) -> Result<ProjectReconcileReport> {
        self.prune_inflight();
        let project = projects::get(self.db.pool(), project_id)
            .await?
            .ok_or_else(|| anyhow!("project {project_id} not found"))?;
        let issues = issues::list_by_project(self.db.pool(), project_id).await?;
        let running = self
            .running
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let known_paths = self
            .known_issue_worktree_paths_for_project(Path::new(&project.repo_path), project.id)
            .await?;
        reconcile::diagnose_project(&project, &issues, &running, &known_paths, dry_run).await
    }

    pub async fn reconcile_project(
        &self,
        project_id: i64,
        now: i64,
    ) -> Result<ProjectReconcileReport> {
        let Some(_tick_guard) = self.try_project_tick(project_id) else {
            bail!("project {project_id} is already reconciling or ticking");
        };
        self.prune_inflight();
        let mut report = self.diagnose_project(project_id, false).await?;

        let project = projects::get(self.db.pool(), project_id)
            .await?
            .ok_or_else(|| anyhow!("project {project_id} not found"))?;
        let repo_path = Path::new(&project.repo_path);
        if !report.orphans.is_empty() {
            let known_paths = self
                .known_issue_worktree_paths_for_project(repo_path, project.id)
                .await?;
            let removed = prune_orphaned_issue_worktrees(repo_path, &known_paths).await?;
            report.applied_count += removed.len();
        }

        for item in report.issues.clone() {
            match item.proposed_action {
                ReconcileActionKind::MarkDone => {
                    self.apply_validated_reconcile_action(
                        project_id,
                        AgentReconcileAction {
                            action: ReconcileActionKind::MarkDone,
                            issue_id: Some(item.issue_id),
                            rationale: None,
                            command: None,
                        },
                        now,
                    )
                    .await?;
                    report.applied_count += 1;
                }
                ReconcileActionKind::ApplyMerge => {
                    self.apply_validated_reconcile_action(
                        project_id,
                        AgentReconcileAction {
                            action: ReconcileActionKind::ApplyMerge,
                            issue_id: Some(item.issue_id),
                            rationale: None,
                            command: None,
                        },
                        now,
                    )
                    .await?;
                    report.applied_count += 1;
                    break;
                }
                _ => {}
            }
        }

        let refreshed = self.diagnose_project(project_id, true).await?;
        if refreshed.agentic_count > 0
            && !main_jobs::has_active_project_kind(self.db.pool(), project_id, "reconcile").await?
        {
            let prompt = reconcile_agent_prompt(&refreshed)?;
            let id = main_jobs::enqueue_project_reconcile(self.db.pool(), project_id, &prompt, now)
                .await?;
            report.queued_main_job_id = Some(id);
            let job = main_job_runner::RoutineJob {
                main_job_id: id,
                routine_id: None,
                output_route: routines::OutputRoute::Report,
                project,
                prompt,
                phase: "reconcile",
            };
            self.spawn_main_job(job);
        }
        Ok(report)
    }

    pub async fn apply_reconcile_job(
        &self,
        main_job_id: i64,
        now: i64,
    ) -> Result<ProjectReconcileReport> {
        let job = main_jobs::get(self.db.pool(), main_job_id)
            .await?
            .ok_or_else(|| anyhow!("main job {main_job_id} not found"))?;
        if job.kind != "reconcile" {
            bail!("main job {main_job_id} is kind {}, not reconcile", job.kind);
        }
        if job.status != MainJobStatus::Done {
            bail!(
                "main job {main_job_id} is {}, not DONE",
                job.status.as_str()
            );
        }
        let Some(_tick_guard) = self.try_project_tick(job.project_id) else {
            bail!(
                "project {} is already reconciling or ticking",
                job.project_id
            );
        };
        let log_path = job
            .log_path
            .as_deref()
            .ok_or_else(|| anyhow!("main job {main_job_id} has no log path"))?;
        let started_at = job
            .started_at
            .ok_or_else(|| anyhow!("main job {main_job_id} has no started_at"))?;
        let expected_log_path = artifacts::main_job_log_path(job.project_id, job.id, started_at)?;
        ensure_expected_artifact_path(Path::new(log_path), &expected_log_path)
            .with_context(|| format!("validating reconcile proposal log {log_path}"))?;
        let text = artifacts::tail_file(PathBuf::from(log_path), 256 * 1024)
            .await
            .with_context(|| format!("reading reconcile proposal log {log_path}"))?;
        let proposal = reconcile::parse_agent_proposal(&text)?;
        let mut report = ProjectReconcileReport::empty(job.project_id, true);
        for action in proposal.actions {
            let applied = action.action != ReconcileActionKind::ManualRequired;
            self.apply_validated_reconcile_action(job.project_id, action, now)
                .await?;
            if applied {
                report.applied_count += 1;
            }
        }
        report.refresh_counts();
        Ok(report)
    }

    fn validate_reconcile_action(
        &self,
        report: &ProjectReconcileReport,
        action: &AgentReconcileAction,
    ) -> Result<()> {
        match action.action {
            ReconcileActionKind::ManualRequired => Ok(()),
            ReconcileActionKind::MarkDone
            | ReconcileActionKind::CleanupWorktree
            | ReconcileActionKind::RetryIssue
            | ReconcileActionKind::ApplyMerge => {
                let issue_id = action.issue_id.ok_or_else(|| {
                    anyhow!("{} action requires issue_id", action.action.as_str())
                })?;
                reconcile::validate_agent_action(report, issue_id, action.action)
                    .with_context(|| format!("stale_proposal: issue {issue_id}"))?;
                if action.action == ReconcileActionKind::RetryIssue && self.is_running(issue_id) {
                    bail!("stale_proposal: issue {issue_id} has an active worker");
                }
                Ok(())
            }
            other => bail!(
                "agent action {} is not accepted from proposals",
                other.as_str()
            ),
        }
    }

    async fn apply_validated_reconcile_action(
        &self,
        project_id: i64,
        action: AgentReconcileAction,
        now: i64,
    ) -> Result<()> {
        let latest = self.diagnose_project(project_id, true).await?;
        self.validate_reconcile_action(&latest, &action)?;
        let Some(issue_id) = action.issue_id else {
            return Ok(());
        };
        match action.action {
            ReconcileActionKind::ManualRequired => Ok(()),
            ReconcileActionKind::MarkDone => {
                let project = projects::get(self.db.pool(), project_id)
                    .await?
                    .ok_or_else(|| anyhow!("project {project_id} not found"))?;
                let latest_item = latest_reconcile_issue(&latest, issue_id)?;
                let issue = self.issue_in_project(project_id, issue_id).await?;
                if issue.status != latest_item.status {
                    bail!(
                        "stale_proposal: issue {issue_id} is {}, not {}",
                        issue.status.as_str(),
                        latest_item.status.as_str()
                    );
                }
                self.mark_reconciled_done(&project, &issue, now).await
            }
            ReconcileActionKind::CleanupWorktree => {
                let project = projects::get(self.db.pool(), project_id)
                    .await?
                    .ok_or_else(|| anyhow!("project {project_id} not found"))?;
                let issue = self.issue_in_project(project_id, issue_id).await?;
                self.cleanup_issue_worktree(&project, &issue).await
            }
            ReconcileActionKind::RetryIssue => {
                let issue = self.issue_in_project(project_id, issue_id).await?;
                if issue.status != IssueStatus::Failed {
                    bail!(
                        "stale_proposal: issue {issue_id} is {}, not FAILED",
                        issue.status.as_str()
                    );
                }
                self.retry_failed_issue(issue_id, now).await
            }
            ReconcileActionKind::ApplyMerge => {
                self.release_reconcile_merge(project_id, issue_id, now)
                    .await
            }
            other => bail!("agent action {} is not applyable", other.as_str()),
        }
    }

    fn ensure_issue_idle(&self, issue_id: i64) -> Result<()> {
        if self.running.lock().unwrap().contains(&issue_id) {
            bail!("issue {issue_id} is already running");
        }
        Ok(())
    }

    async fn issue_in_project(&self, project_id: i64, issue_id: i64) -> Result<Issue> {
        let issue = issues::get(self.db.pool(), issue_id)
            .await?
            .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
        if issue.project_id != project_id {
            bail!("stale_proposal: issue {issue_id} moved to another project");
        }
        Ok(issue)
    }

    async fn mark_reconciled_done(&self, project: &Project, issue: &Issue, now: i64) -> Result<()> {
        issues::force_status_if_current_project(
            self.db.pool(),
            issue.id,
            project.id,
            issue.status,
            IssueStatus::Done,
            now,
        )
        .await?;
        self.emit_issue_status(issue.id, IssueStatus::Done);
        self.cleanup_issue_worktree(project, issue).await
    }

    async fn release_reconcile_merge(
        &self,
        project_id: i64,
        issue_id: i64,
        now: i64,
    ) -> Result<()> {
        self.issue_in_project(project_id, issue_id).await?;
        issues::transition_if_current_project(
            self.db.pool(),
            issue_id,
            project_id,
            IssueStatus::ReadyToMerge,
            IssueStatus::Merging,
            now,
        )
        .await?;
        self.emit_issue_status(issue_id, IssueStatus::Merging);
        Ok(())
    }

    fn emit_issue_status(&self, issue_id: i64, status: IssueStatus) {
        let _ = self.events.send(Event::IssueStatus { issue_id, status });
    }

    pub async fn run_backlog_now(&self, item_id: i64, now: i64) -> Result<i64> {
        let item = backlog::get(self.db.pool(), item_id)
            .await?
            .ok_or_else(|| anyhow!("backlog item {item_id} not found"))?;
        let project = projects::get(self.db.pool(), item.project_id)
            .await?
            .ok_or_else(|| anyhow!("project {} not found", item.project_id))?;
        let routed = routing::route_one_now_semantic(
            &routing::RouteDeps {
                pool: self.db.pool(),
                project: &project,
                executor: self.executor.as_ref(),
                socket: &self.socket,
                now,
            },
            item_id,
        )
        .await?;
        let issue_id = routed.outcome.issue_id();
        self.run_issue_now(issue_id).await?;
        Ok(issue_id)
    }

    /// Remove an issue through the runtime owner: refuse active agents, tear
    /// down any worktree, resolve source backlog rows, then delete DB history.
    pub async fn remove_issue(&self, issue_id: i64, now: i64) -> Result<()> {
        self.prune_inflight();
        if self.running.lock().unwrap().contains(&issue_id) {
            bail!("issue {issue_id} is currently running");
        }
        let pool = self.db.pool();
        let issue = issues::get(pool, issue_id)
            .await?
            .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
        let project = projects::get(pool, issue.project_id)
            .await?
            .ok_or_else(|| anyhow!("project {} not found", issue.project_id))?;
        self.cleanup_issue_worktree(&project, &issue).await?;
        backlog::dismiss_consumed_by_issue(pool, issue_id, now).await?;
        issues::remove(pool, issue_id).await?;
        let _ = self.events.send(Event::IssueRemoved {
            issue_id,
            project_id: issue.project_id,
        });
        Ok(())
    }

    pub async fn abandon_issue(&self, issue_id: i64, now: i64) -> Result<()> {
        self.prune_inflight();
        if self.running.lock().unwrap().contains(&issue_id) {
            bail!("issue {issue_id} is currently running");
        }
        let pool = self.db.pool();
        let issue = issues::get(pool, issue_id)
            .await?
            .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
        if issue.status.is_terminal() {
            return Ok(());
        }
        let project = projects::get(pool, issue.project_id)
            .await?
            .ok_or_else(|| anyhow!("project {} not found", issue.project_id))?;
        self.cleanup_issue_worktree(&project, &issue).await?;
        issues::transition(pool, issue_id, IssueStatus::Abandoned, now).await?;
        let _ = self.events.send(Event::IssueStatus {
            issue_id,
            status: IssueStatus::Abandoned,
        });
        Ok(())
    }

    /// Explicitly remove an issue worktree while keeping the issue row. This is
    /// the operator recovery path for terminal FAILED issues, where the default
    /// policy keeps the worktree available for inspection.
    pub async fn cleanup_issue_worktree_by_id(&self, issue_id: i64) -> Result<()> {
        self.prune_inflight();
        if self.running.lock().unwrap().contains(&issue_id) {
            bail!("issue {issue_id} is currently running");
        }
        let pool = self.db.pool();
        let issue = issues::get(pool, issue_id)
            .await?
            .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
        let project = projects::get(pool, issue.project_id)
            .await?
            .ok_or_else(|| anyhow!("project {} not found", issue.project_id))?;
        self.cleanup_issue_worktree(&project, &issue).await
    }

    pub async fn remove_project(&self, project_id: i64, shallow: bool) -> Result<()> {
        self.prune_inflight();
        let project = projects::get(self.db.pool(), project_id)
            .await?
            .ok_or_else(|| anyhow!("project {project_id} not found"))?;
        let issues = issues::list_by_project(self.db.pool(), project_id).await?;
        if !shallow {
            let running_issue = {
                let running = self.running.lock().unwrap();
                issues
                    .iter()
                    .find(|issue| running.contains(&issue.id))
                    .map(|issue| issue.id)
            };
            if let Some(issue_id) = running_issue {
                bail!(
                    "project {project_id} has running issue {}; use shallow unregister or clean it up first",
                    issue_id
                );
            }
            self.cleanup_project_worktrees(&project, &issues).await?;
        }
        projects::remove(self.db.pool(), project_id).await?;
        Ok(())
    }

    pub async fn ask_project(
        &self,
        project_id: i64,
        mode: AskMode,
        question: String,
    ) -> Result<i64> {
        let pool = self.db.pool();
        let project = projects::get(pool, project_id)
            .await?
            .ok_or_else(|| anyhow!("project {project_id} not found"))?;
        let issues = issues::list_by_project(pool, project_id).await?;
        let backlog = backlog::list_by_project(pool, project_id).await?;
        let routines = routines::list_by_project(pool, project_id).await?;
        let now = self.clock.now_ms();
        let context = ask_context_summary(&project, &issues, &backlog, &routines);
        let prompt = ask_prompt(mode, &question, &context);
        let log_path = artifacts::ask_log_path(project_id, now)?;
        let timeout = Duration::from_secs(project.main_job_timeout_min.max(1) as u64 * 60);
        let cwd = PathBuf::from(&project.repo_path);
        let env = vec![
            ("AUWSX_PROJECT_ID".to_string(), project_id.to_string()),
            ("AUWSX_ASK_MODE".to_string(), mode.as_str().to_string()),
        ];
        let cmd_template = agent::expand_cmd_template(
            &project.main_agent_cmd,
            agent::AgentTemplateVars::main_job(&self.socket),
        );
        let outcome = self
            .executor
            .execute(AgentSpec {
                cmd_template: &cmd_template,
                prompt: &prompt,
                cwd: &cwd,
                log_path: &log_path,
                timeout,
                env: &env,
            })
            .await?;
        let log_text = artifacts::tail_file(log_path.clone(), 64 * 1024)
            .await
            .unwrap_or_default();
        let mut answer =
            extract_answer_from_log(&log_text).unwrap_or_else(|| log_text.trim().to_string());
        if answer.is_empty() {
            answer = format!(
                "Ask command ended with {:?} exit {:?}, but no answer text was captured.",
                outcome.exit_kind, outcome.exit_code
            );
        }
        let answer_id = ask_answers::create(
            pool,
            ask_answers::NewAskAnswer {
                project_id,
                mode,
                question: &question,
                answer: &answer,
                context_summary: Some(&context),
                log_path: Some(&log_path.to_string_lossy()),
            },
            now,
        )
        .await?;
        let _ = self.events.send(Event::AskAnswered {
            answer_id,
            project_id,
        });
        Ok(answer_id)
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
            if let Some(routine_id) = routine_id {
                running_routines.lock().unwrap().remove(&routine_id);
            }
        });
        self.inflight.lock().unwrap().push(handle);
    }

    fn spawn_memory_job(&self, project_id: i64, kind: &'static str, issue_id: Option<i64>) {
        let db = self.db.clone();
        let clock = self.clock.clone();
        let executor = self.executor.clone();
        let events = self.events.clone();
        let socket = self.socket.clone();

        let handle = tokio::spawn(async move {
            let deps = main_job_runner::Deps {
                db: &db,
                clock: &*clock,
                executor: &*executor,
                events: &events,
                socket,
            };
            let job = match main_job_runner::enqueue_memory_job(&deps, project_id, kind, issue_id)
                .await
            {
                Ok(job) => job,
                Err(e) => {
                    tracing::warn!(
                        "memory job {kind} enqueue for project {project_id} failed: {e:#}"
                    );
                    return;
                }
            };
            let main_job_id = job.main_job_id;
            let status = match main_job_runner::execute_routine(&deps, &job).await {
                Ok(status) => status,
                Err(e) => {
                    tracing::warn!("memory job {kind} {main_job_id} failed: {e:#}");
                    MainJobStatus::Failed
                }
            };
            if kind == "deepsleep" && status == MainJobStatus::Done {
                if let Err(e) =
                    projects::mark_deepsleep_ran(db.pool(), project_id, clock.now_ms()).await
                {
                    tracing::warn!(
                        "recording deepsleep completion for project {project_id} failed: {e:#}"
                    );
                }
            }
            let _ = events.send(Event::MainJobStatus {
                main_job_id,
                status,
            });
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
            match issues::get(db.pool(), issue_id).await {
                Ok(Some(issue)) if issue.status == IssueStatus::Done => {
                    let deps = main_job_runner::Deps {
                        db: &db,
                        clock: &*clock,
                        executor: &*executor,
                        events: &events,
                        socket: deps.socket.clone(),
                    };
                    match main_job_runner::enqueue_memory_job(
                        &deps,
                        issue.project_id,
                        "dream",
                        Some(issue_id),
                    )
                    .await
                    {
                        Ok(job) => {
                            let main_job_id = job.main_job_id;
                            let status = match main_job_runner::execute_routine(&deps, &job).await {
                                Ok(status) => status,
                                Err(e) => {
                                    tracing::warn!(
                                        "post-merge dream job {main_job_id} for issue {issue_id} failed: {e:#}"
                                    );
                                    MainJobStatus::Failed
                                }
                            };
                            let _ = events.send(Event::MainJobStatus {
                                main_job_id,
                                status,
                            });
                        }
                        Err(e) => tracing::warn!(
                            "post-merge dream enqueue for issue {issue_id} failed: {e:#}"
                        ),
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("checking issue {issue_id} after phase failed: {e:#}"),
            }
        });
        self.inflight.lock().unwrap().push(handle);
    }

    /// Reconcile interrupted DB state left by a previous daemon exit.
    pub async fn recover_interrupted_work(&self, now: i64) -> Result<usize> {
        let issue_runs = self.recover_open_issue_runs(now).await?;
        let main_jobs = self.recover_open_main_jobs(now).await?;
        Ok(issue_runs + main_jobs)
    }

    /// Close open issue run rows left by a previous daemon and move still
    /// actionable issues to FAILED. This prevents a restart from silently
    /// spawning a second phase for the same issue while the previous run row is
    /// still open.
    pub async fn recover_open_issue_runs(&self, now: i64) -> Result<usize> {
        let pool = self.db.pool();
        let runs = agent_runs::list_open_issue_runs(pool).await?;
        let mut recovered = 0;
        for run in runs {
            let Some(issue_id) = run.issue_id else {
                continue;
            };
            let mut status_after = None;
            if let Some(issue) = issues::get(pool, issue_id).await? {
                let mut status = issue.status;
                if issue.status.is_actionable()
                    && run.status_before.as_deref() == Some(issue.status.as_str())
                    && issues::transition(pool, issue_id, IssueStatus::Failed, now)
                        .await
                        .is_ok()
                {
                    status = IssueStatus::Failed;
                    let _ = self.events.send(Event::IssueStatus {
                        issue_id,
                        status: IssueStatus::Failed,
                    });
                }
                status_after = Some(status.as_str().to_string());
            }
            agent_runs::finish(
                pool,
                run.id,
                status_after.as_deref(),
                None,
                ExitKind::Error,
                now,
                Some("closed during daemon startup recovery"),
            )
            .await?;
            recovered += 1;
        }
        Ok(recovered)
    }

    /// Close interrupted main-job run rows and mark RUNNING main jobs failed.
    /// QUEUED jobs are left queued so routine/user work can be retried.
    pub async fn recover_open_main_jobs(&self, now: i64) -> Result<usize> {
        let pool = self.db.pool();
        let mut recovered = 0;
        let mut recovered_jobs = HashSet::new();
        for run in agent_runs::list_open_main_job_runs(pool).await? {
            let Some(main_job_id) = run.main_job_id else {
                continue;
            };
            let status_after = match main_jobs::get(pool, main_job_id).await? {
                Some(job) if job.status == MainJobStatus::Running => {
                    main_jobs::finish(
                        pool,
                        job.id,
                        MainJobStatus::Failed,
                        now,
                        Some("marked failed during daemon startup recovery"),
                    )
                    .await?;
                    recovered_jobs.insert(job.id);
                    let _ = self.events.send(Event::MainJobStatus {
                        main_job_id: job.id,
                        status: MainJobStatus::Failed,
                    });
                    Some(MainJobStatus::Failed.as_str().to_string())
                }
                Some(job) => Some(job.status.as_str().to_string()),
                None => None,
            };
            agent_runs::finish(
                pool,
                run.id,
                status_after.as_deref(),
                None,
                ExitKind::Error,
                now,
                Some("closed during daemon startup recovery"),
            )
            .await?;
            recovered += 1;
        }

        for job in main_jobs::list_running(pool).await? {
            if recovered_jobs.contains(&job.id) {
                continue;
            }
            main_jobs::finish(
                pool,
                job.id,
                MainJobStatus::Failed,
                now,
                Some("marked failed during daemon startup recovery"),
            )
            .await?;
            recovered += 1;
            let _ = self.events.send(Event::MainJobStatus {
                main_job_id: job.id,
                status: MainJobStatus::Failed,
            });
        }
        Ok(recovered)
    }

    /// Arm a soft gate (set `wait_until`) or release it (transition) when due.
    async fn service_soft_gate(&self, project: &Project, issue_id: i64) -> Result<()> {
        let pool = self.db.pool();
        let Some(issue) = issues::get(pool, issue_id).await? else {
            return Ok(());
        };
        let now = self.clock.now_ms();
        let (target, deadline_min) = match issue.status {
            IssueStatus::PlanReady => (IssueStatus::Working, project.plan_gate_timeout_min),
            IssueStatus::ReadyToMerge => {
                self.ensure_local_merge_not_conflict_blocked(issue.project_id)
                    .await?;
                let mins = match project.completion_policy {
                    CompletionPolicy::Auto => 0,
                    CompletionPolicy::Soft => project.completion_soft_timeout_min,
                    CompletionPolicy::Manual => return Ok(()), // not soft; decide filters this
                };
                (IssueStatus::Merging, mins)
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
        self.cleanup_issue_worktree(project, &issue).await
    }

    async fn cleanup_project_worktrees(&self, project: &Project, issues: &[Issue]) -> Result<()> {
        for issue in issues {
            self.cleanup_issue_worktree(project, issue).await?;
        }

        let repo_path = Path::new(&project.repo_path);
        let known_paths = self
            .known_issue_worktree_paths_for_repo(repo_path, project.id)
            .await?;
        prune_orphaned_issue_worktrees(repo_path, &known_paths).await?;
        Ok(())
    }

    async fn known_issue_worktree_paths_for_repo(
        &self,
        repo_path: &Path,
        excluding_project_id: i64,
    ) -> Result<HashMap<i64, PathBuf>> {
        let mut known = HashMap::new();
        for project in projects::list(self.db.pool()).await? {
            if project.id == excluding_project_id
                || !same_repo_path(Path::new(&project.repo_path), repo_path)
            {
                continue;
            }
            for issue in issues::list_by_project(self.db.pool(), project.id).await? {
                if let Some(path) = issue.worktree_path {
                    known.insert(issue.id, PathBuf::from(path));
                }
            }
        }
        Ok(known)
    }

    async fn known_issue_worktree_paths_for_project(
        &self,
        repo_path: &Path,
        project_id: i64,
    ) -> Result<HashMap<i64, PathBuf>> {
        let mut known = self
            .known_issue_worktree_paths_for_repo(repo_path, -1)
            .await?;
        for issue in issues::list_by_project(self.db.pool(), project_id).await? {
            if let Some(path) = issue.worktree_path {
                known.insert(issue.id, PathBuf::from(path));
            }
        }
        Ok(known)
    }

    async fn cleanup_issue_worktree(&self, project: &Project, issue: &Issue) -> Result<()> {
        let Some(path) = issue.worktree_path.clone() else {
            return Ok(());
        };
        let handle = WorktreeHandle {
            branch: issue
                .branch
                .clone()
                .unwrap_or_else(|| branch_for_issue(issue.id)),
            path: PathBuf::from(path),
        };
        self.worktrees.teardown(project, &handle).await?;
        issues::set_worktree(
            self.db.pool(),
            issue.id,
            None,
            None,
            None,
            self.clock.now_ms(),
        )
        .await
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

fn same_repo_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn ensure_expected_artifact_path(actual: &Path, expected: &Path) -> Result<()> {
    let actual = actual
        .canonicalize()
        .with_context(|| format!("canonicalizing artifact path {}", actual.display()))?;
    let expected = expected.canonicalize().with_context(|| {
        format!(
            "canonicalizing expected artifact path {}",
            expected.display()
        )
    })?;
    if actual != expected {
        bail!(
            "main job log path {} does not match expected artifact path {}",
            actual.display(),
            expected.display()
        );
    }
    Ok(())
}

fn latest_reconcile_issue(
    report: &ProjectReconcileReport,
    issue_id: i64,
) -> Result<&reconcile::ReconcileIssueReport> {
    report
        .issues
        .iter()
        .find(|issue| issue.issue_id == issue_id)
        .ok_or_else(|| anyhow!("stale_proposal: issue {issue_id} is absent"))
}

fn picked_summary(
    triaged_issue_ids: &[i64],
    routes: &[routing::RouteOneResult],
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
        triaged_routes: routes
            .iter()
            .map(|route| SchedulerRunRoute {
                backlog_item_id: route.outcome.item_id(),
                issue_id: route.outcome.issue_id(),
                kind: route.outcome.kind().to_string(),
                fallback_reason: route.fallback_reason.clone(),
            })
            .collect(),
        decisions,
        pending_backlog,
        ready_backlog,
        running_issues,
        max_concurrency,
    }
}

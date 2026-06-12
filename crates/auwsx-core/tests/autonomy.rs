//! Integration tests for the autonomy core: the pure pipeline/scheduler/prompt
//! decision fns, plus a full end-to-end "drive" that carries one issue from
//! CONSOLIDATING to DONE using a FAKE agent (no real processes, no git).
//!
//! Source of truth:
//!   - crates/auwsx-core/src/pipeline.rs    (`plan_phase`, `execute`)
//!   - crates/auwsx-core/src/scheduler.rs   (`decide`, `Decision`, `Scheduler`)
//!   - crates/auwsx-core/src/prompt.rs      (`build`, `PromptContext`)
//!   - crates/auwsx-core/src/worktree.rs    (`branch_for_issue`, `Worktrees`)
//!   - crates/auwsx-core/src/agent/mod.rs   (`AgentExecutor`, `AgentOutcome`, `ExitKind`)
//!   - crates/auwsx-core/src/state.rs       (`IssueStatus`, scheduler classes)
//!
//! These assert the PUBLIC CONTRACT only. The pure fns are hit directly and
//! aggressively (every status row, both capacity boundaries, every gate policy).
//! The drive test exercises the real `Scheduler` runtime against test doubles
//! for the two non-deterministic ports (agent + worktree) and the clock.

use async_trait::async_trait;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use auwsx_core::agent::{AgentExecutor, AgentOutcome, AgentSpec, ExitKind};
use auwsx_core::artifacts;
use auwsx_core::backlog::{self, Source};
use auwsx_core::clock::Clock;
use auwsx_core::db::agent_runs::{self, Role};
use auwsx_core::db::issues::{self, Issue};
use auwsx_core::db::projects::{self, CompletionPolicy, MergeMode, NewProject, Project};
use auwsx_core::db::scheduler_runs;
use auwsx_core::db::Db;
use auwsx_core::events;
use auwsx_core::main_jobs::{self, MainJobStatus};
use auwsx_core::prompt::{self, PromptContext};
use auwsx_core::routines::{self, NewRoutine, RoutineType};
use auwsx_core::scheduler::{decide, Decision, Scheduler};
use auwsx_core::state::IssueStatus;
use auwsx_core::worktree::{branch_for_issue, WorktreeHandle, Worktrees};
use sqlx::SqlitePool;
use tokio::sync::{Mutex as AsyncMutex, Notify};

/// Fixed deterministic timestamp (Unix epoch ms). No SystemTime anywhere.
const TS: i64 = 1_000_000;
static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

// ===========================================================================
// In-memory struct builders for the PURE fns (no DB round-trip needed).
//
// `Issue` and `Project` are plain public structs; build a valid default then
// override only the fields a given test cares about.
// ===========================================================================

/// A plausible default `Issue` in `status`. Worktree/wait fields default empty;
/// override per test.
fn issue_at(id: i64, status: IssueStatus) -> Issue {
    Issue {
        id,
        project_id: 1,
        title: "t".to_string(),
        description: None,
        status,
        branch: None,
        worktree_path: None,
        agent_session: None,
        review_round: 0,
        conflict_attempts: 0,
        wait_until: None,
        absorbed_into_id: None,
        has_pending_steering: false,
        created_at: TS,
        updated_at: TS,
    }
}

/// A plausible default `Project` with the given concurrency + completion policy.
/// `plan_gate_timeout_min`/`completion_soft_timeout_min` default to the SQL
/// defaults; override only when a soft-gate timing test needs it.
fn project_with(max_concurrency: i64, completion_policy: CompletionPolicy) -> Project {
    Project {
        id: 1,
        name: "p".to_string(),
        repo_path: "/repo".to_string(),
        default_branch: "main".to_string(),
        main_agent_cmd: "main {prompt}".to_string(),
        plan_agent_cmd: "plan {prompt}".to_string(),
        work_agent_cmd: "work {prompt}".to_string(),
        review_agent_cmd: None,
        completion_policy,
        completion_soft_timeout_min: 60,
        plan_gate_timeout_min: 10,
        iteration_timeout_min: 30,
        main_job_timeout_min: 60,
        review_max_rounds: 5,
        conflict_max_attempts: 3,
        max_concurrency,
        schedule_interval_min: None,
        merge_mode: MergeMode::Local,
        skill_path: None,
        deepsleep_interval_days: 7,
        last_deepsleep_at: None,
        created_at: TS,
    }
}

fn empty_running() -> HashSet<i64> {
    HashSet::new()
}

// ===========================================================================
// pipeline::plan_phase  — pure: status -> Option<(Role, needs_worktree)>
// ===========================================================================

#[test]
fn given_consolidating_when_plan_phase_then_main_no_worktree() {
    assert_eq!(
        prompt_plan(IssueStatus::Consolidating),
        Some((Role::Main, false))
    );
}

#[test]
fn given_planning_when_plan_phase_then_plan_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::Planning), Some((Role::Plan, true)));
}

#[test]
fn given_implementing_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(
        prompt_plan(IssueStatus::Implementing),
        Some((Role::Work, true))
    );
}

#[test]
fn given_needs_fix_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::NeedsFix), Some((Role::Work, true)));
}

#[test]
fn given_audit_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::Audit), Some((Role::Work, true)));
}

#[test]
fn given_conflicted_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(
        prompt_plan(IssueStatus::Conflicted),
        Some((Role::Work, true))
    );
}

#[test]
fn given_completing_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(
        prompt_plan(IssueStatus::Completing),
        Some((Role::Work, true))
    );
}

#[test]
fn given_review_when_plan_phase_then_review_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::Review), Some((Role::Review, true)));
}

#[test]
fn given_each_human_gated_or_terminal_status_when_plan_phase_then_none() {
    for s in [
        IssueStatus::Planned,
        IssueStatus::PlanBlocked,
        IssueStatus::ReviewBlocked,
        IssueStatus::ConflictBlocked,
        IssueStatus::Ended,
        IssueStatus::Done,
        IssueStatus::Absorbed,
        IssueStatus::Failed,
    ] {
        assert_eq!(prompt_plan(s), None, "{s:?} must not be actionable");
    }
}

#[test]
fn given_representative_statuses_when_plan_phase_then_some_iff_actionable() {
    // The Some-set is exactly the actionable set (pipeline.rs doc contract).
    for s in [
        IssueStatus::Consolidating,
        IssueStatus::Planning,
        IssueStatus::Planned,
        IssueStatus::Implementing,
        IssueStatus::Review,
        IssueStatus::NeedsFix,
        IssueStatus::Audit,
        IssueStatus::Conflicted,
        IssueStatus::Completing,
        IssueStatus::Ended,
        IssueStatus::Done,
        IssueStatus::Failed,
    ] {
        assert_eq!(
            auwsx_core::pipeline::plan_phase(s).is_some(),
            s.is_actionable(),
            "plan_phase/is_actionable disagree for {s:?}"
        );
    }
}

/// Tiny shim so the assertions above read cleanly.
fn prompt_plan(s: IssueStatus) -> Option<(Role, bool)> {
    auwsx_core::pipeline::plan_phase(s)
}

// ===========================================================================
// scheduler::decide  — pure: issues + project + running + now -> Vec<Decision>
// ===========================================================================

#[test]
fn given_actionable_issue_not_running_with_capacity_when_decide_then_spawn() {
    let issues = [issue_at(7, IssueStatus::Implementing)];
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::Spawn(7)]);
}

#[test]
fn given_actionable_issue_already_running_when_decide_then_no_decision() {
    let issues = [issue_at(7, IssueStatus::Implementing)];
    let proj = project_with(1, CompletionPolicy::Manual);
    let mut running = HashSet::new();
    running.insert(7);
    let got = decide(&issues, &proj, &running, TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_max_concurrency_1_and_one_running_when_second_actionable_then_no_spawn() {
    // Issue 1 running; issue 2 actionable + idle. Cap 1 => no free slot.
    let issues = [
        issue_at(1, IssueStatus::Implementing),
        issue_at(2, IssueStatus::Implementing),
    ];
    let proj = project_with(1, CompletionPolicy::Manual);
    let mut running = HashSet::new();
    running.insert(1);
    let got = decide(&issues, &proj, &running, TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_max_concurrency_2_and_zero_running_when_two_actionable_then_two_spawns() {
    let issues = [
        issue_at(1, IssueStatus::Implementing),
        issue_at(2, IssueStatus::Planning),
    ];
    let proj = project_with(2, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::Spawn(1), Decision::Spawn(2)]);
}

#[test]
fn given_planned_issue_with_no_wait_until_when_decide_then_soft_gate() {
    // PLANNED is always a soft gate and starts unarmed (wait_until None).
    let issues = [issue_at(3, IssueStatus::Planned)];
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::SoftGate(3)]);
}

#[test]
fn given_planned_issue_armed_and_now_past_deadline_when_decide_then_soft_gate() {
    let mut issue = issue_at(3, IssueStatus::Planned);
    issue.wait_until = Some(TS); // deadline == now => due (now >= w)
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&[issue], &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::SoftGate(3)]);
}

#[test]
fn given_planned_issue_armed_and_now_before_deadline_when_decide_then_no_decision() {
    let mut issue = issue_at(3, IssueStatus::Planned);
    issue.wait_until = Some(TS + 1); // not yet due
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&[issue], &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_ended_issue_under_auto_policy_when_decide_then_soft_gate() {
    let issues = [issue_at(4, IssueStatus::Ended)];
    let proj = project_with(1, CompletionPolicy::Auto);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::SoftGate(4)]);
}

#[test]
fn given_ended_issue_under_soft_policy_unarmed_when_decide_then_soft_gate() {
    // wait_until None => needs arming, so it surfaces even under soft policy.
    let issues = [issue_at(4, IssueStatus::Ended)];
    let proj = project_with(1, CompletionPolicy::Soft);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::SoftGate(4)]);
}

#[test]
fn given_ended_issue_under_manual_policy_when_decide_then_no_decision() {
    let issues = [issue_at(4, IssueStatus::Ended)];
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_plan_blocked_issue_when_decide_then_no_decision() {
    let issues = [issue_at(5, IssueStatus::PlanBlocked)];
    let proj = project_with(1, CompletionPolicy::Auto);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_review_blocked_issue_when_decide_then_no_decision() {
    let issues = [issue_at(5, IssueStatus::ReviewBlocked)];
    let proj = project_with(1, CompletionPolicy::Auto);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_conflict_blocked_issue_when_decide_then_no_decision() {
    let issues = [issue_at(5, IssueStatus::ConflictBlocked)];
    let proj = project_with(1, CompletionPolicy::Auto);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_done_issue_with_worktree_when_decide_then_teardown() {
    let mut issue = issue_at(8, IssueStatus::Done);
    issue.worktree_path = Some("/wt".to_string());
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&[issue], &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::Teardown(8)]);
}

#[test]
fn given_done_issue_without_worktree_when_decide_then_no_decision() {
    let issue = issue_at(8, IssueStatus::Done); // worktree_path None
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&[issue], &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_failed_issue_with_worktree_when_decide_then_no_decision() {
    let mut issue = issue_at(8, IssueStatus::Failed);
    issue.worktree_path = Some("/wt".to_string());
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&[issue], &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_absorbed_issue_with_worktree_when_decide_then_no_decision() {
    let mut issue = issue_at(8, IssueStatus::Absorbed);
    issue.worktree_path = Some("/wt".to_string());
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&[issue], &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

// ===========================================================================
// prompt::build  — actionable => Some(callback-bearing text); else None
// ===========================================================================

#[test]
fn given_planning_issue_when_build_then_some_contains_id_and_callback() {
    let issue = issue_at(42, IssueStatus::Planning);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
    };
    let text = prompt::build(&ctx).expect("actionable status yields a prompt");
    assert!(text.contains("42"), "prompt must name the issue id");
    assert!(
        text.contains("auwsx issue status"),
        "prompt must carry the control-CLI callback"
    );
}

#[test]
fn given_done_issue_when_build_then_none() {
    let issue = issue_at(42, IssueStatus::Done);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
    };
    assert!(
        prompt::build(&ctx).is_none(),
        "non-actionable status yields no prompt"
    );
}

#[test]
fn given_planning_issue_when_build_then_body_mentions_planned_callback() {
    let issue = issue_at(1, IssueStatus::Planning);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
    };
    let text = prompt::build(&ctx).expect("planning prompt");
    assert!(
        text.contains("PLANNED"),
        "planning body must point at the PLANNED target"
    );
}

#[test]
fn given_review_issue_when_build_then_body_mentions_needs_fix_and_audit() {
    let issue = issue_at(1, IssueStatus::Review);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
    };
    let text = prompt::build(&ctx).expect("review prompt");
    assert!(
        text.contains("NEEDS_FIX"),
        "review body must offer NEEDS_FIX"
    );
    assert!(text.contains("AUDIT"), "review body must offer AUDIT");
}

// ===========================================================================
// worktree::branch_for_issue
// ===========================================================================

#[test]
fn given_issue_id_when_branch_for_issue_then_matches_template() {
    assert_eq!(branch_for_issue(17), "auwsx/issue-17");
}

// ===========================================================================
// The DRIVE test: the Scheduler carries one issue CONSOLIDATING -> DONE using a
// fake agent (status transitions only) and a temp-dir worktree (no git).
//
// Agent-driven edges:   CONSOLIDATING->PLANNING, PLANNING->PLANNED,
//                       IMPLEMENTING->REVIEW, REVIEW->AUDIT, AUDIT->ENDED,
//                       COMPLETING->DONE.
// Scheduler soft-gate:  PLANNED->IMPLEMENTING, ENDED->COMPLETING.
//
// Env note: `AUWSX_DATA_DIR` is process-global and tests run in parallel. We
// consolidate ALL drive assertions into this ONE test, set the data dir once to
// a tempdir held for the whole test, and never race a second drive on a
// different value.
// ===========================================================================

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// No git: just hand back the temp path it was constructed with.
struct FakeWorktrees(PathBuf);
#[async_trait]
impl Worktrees for FakeWorktrees {
    async fn create(&self, _p: &Project, branch: &str) -> anyhow::Result<WorktreeHandle> {
        Ok(WorktreeHandle {
            branch: branch.to_string(),
            path: self.0.clone(),
        })
    }
    async fn teardown(&self, _p: &Project, _h: &WorktreeHandle) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Reads `AUWSX_ISSUE_ID` from the spec env, applies the single transition a
/// real agent would request via the control CLI for the current phase, then
/// "exits" cleanly. PLANNED->IMPLEMENTING and ENDED->COMPLETING are the
/// scheduler's soft gates, NOT the agent's job.
struct ScriptedAgent {
    db: Db,
    now: i64,
}
#[async_trait]
impl AgentExecutor for ScriptedAgent {
    async fn execute(&self, spec: AgentSpec<'_>) -> anyhow::Result<AgentOutcome> {
        let issue_id: i64 = spec
            .env
            .iter()
            .find(|(k, _)| k == "AUWSX_ISSUE_ID")
            .expect("pipeline injects AUWSX_ISSUE_ID")
            .1
            .parse()
            .expect("issue id parses");
        let cur = issues::get(self.db.pool(), issue_id)
            .await?
            .expect("issue exists during run")
            .status;
        let next = match cur {
            IssueStatus::Consolidating => Some(IssueStatus::Planning),
            IssueStatus::Planning => Some(IssueStatus::Planned),
            IssueStatus::Implementing => Some(IssueStatus::Review),
            IssueStatus::Review => Some(IssueStatus::Audit), // clean review, no findings
            IssueStatus::Audit => Some(IssueStatus::Ended),
            IssueStatus::Completing => Some(IssueStatus::Done),
            _ => None,
        };
        if let Some(n) = next {
            issues::transition(self.db.pool(), issue_id, n, self.now).await?;
        }
        Ok(AgentOutcome {
            exit_kind: ExitKind::Exited,
            exit_code: Some(0),
            pid: None,
        })
    }
}

struct ExitAgent;
#[async_trait]
impl AgentExecutor for ExitAgent {
    async fn execute(&self, _spec: AgentSpec<'_>) -> anyhow::Result<AgentOutcome> {
        Ok(AgentOutcome {
            exit_kind: ExitKind::Exited,
            exit_code: Some(0),
            pid: None,
        })
    }
}

struct ErrorAgent;
#[async_trait]
impl AgentExecutor for ErrorAgent {
    async fn execute(&self, _spec: AgentSpec<'_>) -> anyhow::Result<AgentOutcome> {
        anyhow::bail!("executor setup failed")
    }
}

struct BlockingAgent {
    started: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl AgentExecutor for BlockingAgent {
    async fn execute(&self, _spec: AgentSpec<'_>) -> anyhow::Result<AgentOutcome> {
        self.started.notify_waiters();
        self.release.notified().await;
        Ok(AgentOutcome {
            exit_kind: ExitKind::Exited,
            exit_code: Some(0),
            pid: None,
        })
    }
}

/// Create a project whose gates auto-release with no time travel:
/// completion_policy='auto', plan_gate_timeout_min=0.
async fn drive_project(pool: &SqlitePool) -> anyhow::Result<i64> {
    let id = projects::create(
        pool,
        NewProject {
            name: "drive",
            repo_path: "/repo",
            default_branch: "main",
            main_agent_cmd: "main {prompt}",
            plan_agent_cmd: "plan {prompt}",
            work_agent_cmd: "work {prompt}",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Auto),
            plan_gate_timeout_min: Some(0),
            completion_soft_timeout_min: None,
        },
        TS,
    )
    .await?;
    Ok(id)
}

async fn set_project_runtime_policy(
    pool: &SqlitePool,
    project_id: i64,
    max_concurrency: i64,
    schedule_interval_min: Option<i64>,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE projects
         SET max_concurrency = ?, schedule_interval_min = ?
         WHERE id = ?",
    )
    .bind(max_concurrency)
    .bind(schedule_interval_min)
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn scheduler_with(
    db: Db,
    clock: Arc<dyn Clock>,
    executor: Arc<dyn AgentExecutor>,
    tick_interval: Duration,
) -> Scheduler {
    Scheduler::new(
        db,
        clock,
        executor,
        Arc::new(FakeWorktrees(PathBuf::from("/tmp/auwsx-test-worktree"))),
        events::channel(),
        PathBuf::from("/tmp/unused.sock"),
        tick_interval,
    )
}

async fn wait_for_scheduler_runs(
    pool: &SqlitePool,
    project_id: i64,
    min: usize,
) -> anyhow::Result<Vec<auwsx_core::db::scheduler_runs::SchedulerRun>> {
    for _ in 0..100 {
        let runs = scheduler_runs::recent_by_project(pool, project_id, 100).await?;
        if runs.len() >= min {
            return Ok(runs);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    scheduler_runs::recent_by_project(pool, project_id, 100)
        .await
        .map_err(Into::into)
}

async fn wait_for_agent_runs(
    pool: &SqlitePool,
    project_id: i64,
    min: usize,
) -> anyhow::Result<Vec<auwsx_core::db::agent_runs::AgentRun>> {
    for _ in 0..100 {
        let runs = agent_runs::recent_by_project(pool, project_id, 100).await?;
        if runs.len() >= min {
            return Ok(runs);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    agent_runs::recent_by_project(pool, project_id, 100)
        .await
        .map_err(Into::into)
}

#[tokio::test]
async fn given_fake_agent_when_scheduler_ticks_then_issue_reaches_done_and_worktree_torn_down(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "drive me", None, TS).await?;

    // Per-test data dir for pipeline run-logs/prompts. Held for the whole test.
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());
    let wt_tmp = tempfile::tempdir()?;

    let bus = events::channel();
    let sched = Scheduler::new(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ScriptedAgent {
            db: db.clone(),
            now: TS,
        }),
        Arc::new(FakeWorktrees(wt_tmp.path().to_path_buf())),
        bus,
        PathBuf::from("/tmp/unused.sock"),
        Duration::from_secs(60),
    );

    // Drive ticks by hand (do NOT call sched.run).
    let mut status = IssueStatus::Consolidating;
    for _ in 0..30 {
        sched.tick_project(project_id).await?;
        sched.join_inflight().await;
        status = issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status;
        if status == IssueStatus::Done {
            break;
        }
    }
    assert_eq!(
        status,
        IssueStatus::Done,
        "scheduler must drive the issue to DONE"
    );

    // One more pass: DONE + worktree present => Teardown clears the worktree.
    sched.tick_project(project_id).await?;
    sched.join_inflight().await;
    let final_issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    assert_eq!(
        final_issue.worktree_path, None,
        "Teardown must clear the worktree after DONE"
    );

    // The pipeline recorded each spawn: at least one run, and every FINISHED run
    // exited cleanly with a recorded status_after.
    let runs = agent_runs::list_by_issue(db.pool(), issue_id).await?;
    assert!(!runs.is_empty(), "pipeline must record agent runs");
    for r in &runs {
        // join_inflight awaited every spawned task, so all runs are finished.
        assert_eq!(
            r.exit_kind,
            Some(ExitKind::Exited),
            "run {} not finished cleanly",
            r.id
        );
        assert!(
            r.status_after.is_some(),
            "run {} missing status_after",
            r.id
        );
    }

    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_approved_backlog_when_tick_project_then_backlog_is_consumed_into_issue(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, None).await?;
    let backlog_id = backlog::add(
        db.pool(),
        project_id,
        "approved item",
        Source::Human,
        None,
        TS,
    )
    .await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    sched.tick_project(project_id).await?;

    let item = backlog::get(db.pool(), backlog_id)
        .await?
        .expect("backlog item exists");
    let issue_count = issues::list_by_project(db.pool(), project_id).await?.len();
    let run_count = scheduler_runs::recent_by_project(db.pool(), project_id, 10)
        .await?
        .len();
    assert_eq!(
        (item.consumed_issue_id.is_some(), issue_count, run_count),
        (true, 1, 1)
    );
    Ok(())
}

#[tokio::test]
async fn given_pending_backlog_when_tick_project_then_backlog_is_not_consumed() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, None).await?;
    let backlog_id = backlog::add(
        db.pool(),
        project_id,
        "pending item",
        Source::Agent,
        None,
        TS,
    )
    .await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    sched.tick_project(project_id).await?;

    let item = backlog::get(db.pool(), backlog_id)
        .await?
        .expect("backlog item exists");
    let issue_count = issues::list_by_project(db.pool(), project_id).await?.len();
    assert_eq!((item.consumed_issue_id, issue_count), (None, 0));
    Ok(())
}

#[tokio::test]
async fn given_pending_backlog_when_run_backlog_now_then_promotes_and_spawns_first_phase(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 1, None).await?;
    let backlog_id = backlog::add(
        db.pool(),
        project_id,
        "manual backlog",
        Source::Agent,
        None,
        TS,
    )
    .await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let issue_id = sched.run_backlog_now(backlog_id, TS).await?;
    sched.join_inflight().await;

    let item = backlog::get(db.pool(), backlog_id)
        .await?
        .expect("backlog exists");
    let runs = agent_runs::list_by_issue(db.pool(), issue_id).await?;
    assert_eq!(item.consumed_issue_id, Some(issue_id));
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].role, Role::Main);
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_routine_when_run_routine_now_then_main_job_and_agent_run_are_recorded(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let routine_id = routines::create(
        db.pool(),
        NewRoutine {
            project_id,
            name: "daily report",
            routine_type: RoutineType::Report,
            prompt: "write a report",
            cron: "0 0 * * * *",
            writable_paths: None,
            enabled: true,
        },
        TS,
    )
    .await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let main_job_id = sched.run_routine_now(routine_id).await?;
    sched.join_inflight().await;

    let job = main_jobs::get(db.pool(), main_job_id)
        .await?
        .expect("main job exists");
    let runs = agent_runs::recent_by_project(db.pool(), project_id, 10).await?;
    let routine = routines::get(db.pool(), routine_id)
        .await?
        .expect("routine exists");
    let expected_log = artifacts::main_job_log_path(project_id, main_job_id, TS)?
        .to_string_lossy()
        .to_string();
    assert_eq!(job.status, MainJobStatus::Done);
    assert_eq!(job.routine_id, Some(routine_id));
    assert_eq!(job.log_path.as_deref(), Some(expected_log.as_str()));
    assert_eq!(routine.last_run_at, Some(TS));
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].main_job_id, Some(main_job_id));
    assert_eq!(runs[0].role, Role::Main);
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_more_actionable_issues_than_capacity_when_tick_project_then_spawns_up_to_max_concurrency(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 2, None).await?;
    issues::create(db.pool(), project_id, "one", None, TS).await?;
    issues::create(db.pool(), project_id, "two", None, TS).await?;
    issues::create(db.pool(), project_id, "three", None, TS).await?;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(BlockingAgent {
            started: started.clone(),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );

    let started_wait = started.notified();
    sched.tick_project(project_id).await?;
    started_wait.await;
    let runs = wait_for_agent_runs(db.pool(), project_id, 2).await?;

    assert_eq!(runs.len(), 2);
    release.notify_waiters();
    sched.join_inflight().await;
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_issue_running_in_other_project_when_tick_project_then_capacity_is_project_local(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let first_project_id = drive_project(db.pool()).await?;
    let second_project_id = projects::create(
        db.pool(),
        NewProject {
            name: "second",
            repo_path: "/repo2",
            default_branch: "main",
            main_agent_cmd: "main {prompt}",
            plan_agent_cmd: "plan {prompt}",
            work_agent_cmd: "work {prompt}",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Auto),
            plan_gate_timeout_min: Some(0),
            completion_soft_timeout_min: None,
        },
        TS,
    )
    .await?;
    set_project_runtime_policy(db.pool(), first_project_id, 1, None).await?;
    set_project_runtime_policy(db.pool(), second_project_id, 1, None).await?;
    issues::create(db.pool(), first_project_id, "one", None, TS).await?;
    issues::create(db.pool(), second_project_id, "two", None, TS).await?;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(BlockingAgent {
            started: started.clone(),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );

    let started_wait = started.notified();
    sched.tick_project(first_project_id).await?;
    started_wait.await;
    sched.tick_project(second_project_id).await?;
    let runs = wait_for_agent_runs(db.pool(), second_project_id, 1).await?;

    assert_eq!(runs.len(), 1);
    release.notify_waiters();
    sched.join_inflight().await;
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_project_without_interval_when_run_ticks_then_project_is_manual_only(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, None).await?;
    let shutdown = Arc::new(Notify::new());
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_millis(10),
    );
    let worker = sched.clone();
    let signal = shutdown.clone();
    let handle = tokio::spawn(async move {
        worker.run(signal).await;
    });

    tokio::time::sleep(Duration::from_millis(40)).await;
    handle.abort();
    let _ = handle.await;

    let runs = scheduler_runs::recent_by_project(db.pool(), project_id, 10).await?;
    assert_eq!(runs.len(), 0);
    Ok(())
}

#[tokio::test]
async fn given_project_zero_interval_when_run_ticks_then_scheduler_records_repeated_passes(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, Some(0)).await?;
    let shutdown = Arc::new(Notify::new());
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_millis(10),
    );
    let worker = sched.clone();
    let signal = shutdown.clone();
    let handle = tokio::spawn(async move {
        worker.run(signal).await;
    });

    let runs = wait_for_scheduler_runs(db.pool(), project_id, 2).await?;
    handle.abort();
    let _ = handle.await;

    assert!(runs.len() >= 2);
    Ok(())
}

#[tokio::test]
async fn given_project_interval_not_elapsed_when_run_ticks_then_project_is_not_scheduled(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, Some(60)).await?;
    scheduler_runs::record(
        db.pool(),
        project_id,
        TS,
        scheduler_runs::SchedulerRunSource::Auto,
        Some("{}"),
    )
    .await?;
    let backlog_id = backlog::add(
        db.pool(),
        project_id,
        "approved item",
        Source::Human,
        None,
        TS,
    )
    .await?;
    let shutdown = Arc::new(Notify::new());
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_millis(10),
    );
    let worker = sched.clone();
    let signal = shutdown.clone();
    let handle = tokio::spawn(async move {
        worker.run(signal).await;
    });

    tokio::time::sleep(Duration::from_millis(40)).await;
    handle.abort();
    let _ = handle.await;

    let item = backlog::get(db.pool(), backlog_id)
        .await?
        .expect("backlog item exists");
    let run_count = scheduler_runs::recent_by_project(db.pool(), project_id, 10)
        .await?
        .len();
    assert_eq!((item.consumed_issue_id, run_count), (None, 1));
    Ok(())
}

#[tokio::test]
async fn given_project_interval_elapsed_when_run_ticks_then_project_is_scheduled(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, Some(1)).await?;
    scheduler_runs::record(
        db.pool(),
        project_id,
        TS - 60_000,
        scheduler_runs::SchedulerRunSource::Auto,
        Some("{}"),
    )
    .await?;
    let backlog_id = backlog::add(
        db.pool(),
        project_id,
        "approved item",
        Source::Human,
        None,
        TS,
    )
    .await?;
    let shutdown = Arc::new(Notify::new());
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_millis(10),
    );
    let worker = sched.clone();
    let signal = shutdown.clone();
    let handle = tokio::spawn(async move {
        worker.run(signal).await;
    });

    let _ = wait_for_scheduler_runs(db.pool(), project_id, 2).await?;
    handle.abort();
    let _ = handle.await;

    let item = backlog::get(db.pool(), backlog_id)
        .await?
        .expect("backlog item exists");
    assert!(item.consumed_issue_id.is_some());
    Ok(())
}

#[tokio::test]
async fn given_project_interval_configured_when_manual_tick_project_then_interval_is_bypassed(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, Some(60)).await?;
    scheduler_runs::record(
        db.pool(),
        project_id,
        TS,
        scheduler_runs::SchedulerRunSource::Auto,
        Some("{}"),
    )
    .await?;
    let backlog_id = backlog::add(
        db.pool(),
        project_id,
        "approved item",
        Source::Human,
        None,
        TS,
    )
    .await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    sched.tick_project(project_id).await?;

    let item = backlog::get(db.pool(), backlog_id)
        .await?
        .expect("backlog item exists");
    let run_count = scheduler_runs::recent_by_project(db.pool(), project_id, 10)
        .await?
        .len();
    assert_eq!((item.consumed_issue_id.is_some(), run_count), (true, 2));
    Ok(())
}

#[tokio::test]
async fn given_executor_error_when_tick_project_then_run_is_finished_and_issue_failed(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "fails", None, TS).await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ErrorAgent),
        Duration::from_secs(60),
    );

    sched.tick_project(project_id).await?;
    sched.join_inflight().await;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    let runs = agent_runs::list_by_issue(db.pool(), issue_id).await?;
    assert_eq!(
        (
            issue.status,
            runs.len(),
            runs.first().and_then(|run| run.exit_kind)
        ),
        (IssueStatus::Failed, 1, Some(ExitKind::Error))
    );
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_issue_already_running_when_run_issue_now_then_call_fails() -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "run now", None, TS).await?;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(BlockingAgent {
            started: started.clone(),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );
    let started_wait = started.notified();
    sched.run_issue_now(issue_id).await?;
    started_wait.await;

    let err = sched
        .run_issue_now(issue_id)
        .await
        .expect_err("second run must fail");

    assert!(err.to_string().contains("already running"));
    release.notify_waiters();
    sched.join_inflight().await;
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

//! Integration tests for the autonomy core: the pure pipeline/scheduler/prompt
//! decision fns, plus a full end-to-end "drive" that carries one issue from
//! NEW to DONE using a FAKE agent (no real processes, no git).
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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auwsx_core::agent::{AgentExecutor, AgentOutcome, AgentSpec, ExitKind};
use auwsx_core::artifacts;
use auwsx_core::backlog::{self, Source};
use auwsx_core::clock::Clock;
use auwsx_core::control_outbox;
use auwsx_core::db::agent_runs::{self, Role, StartRun};
use auwsx_core::db::issues::{self, Issue};
use auwsx_core::db::projects::{self, CompletionPolicy, MergeMode, NewProject, Project};
use auwsx_core::db::remote::{
    self, ProjectRemoteConfig, RemoteAuthKind, RemotePrCheckStatus, RemotePrLink, RemotePrState,
    RemoteProvider, RemoteSyncDirection, RemoteSyncKind, RemoteSyncStatus, RequiredChecksPolicy,
    UpsertProjectRemoteConfig, UpsertRemotePrLink,
};
use auwsx_core::db::scheduler_runs;
use auwsx_core::db::Db;
use auwsx_core::events;
use auwsx_core::issue_control::ControlOutcome;
use auwsx_core::main_jobs::{self, MainJobStatus};
use auwsx_core::prompt::{self, MemoryInvocation, PromptContext};
use auwsx_core::remote_executor::{
    CreatedRemoteIssue, CreatedRemotePr, RemoteProviderEffect, RemoteProviderExecutor,
    RemoteSyncRequest,
};
use auwsx_core::remote_plan::RemotePlannedAction;
use auwsx_core::routines::{self, NewRoutine, RoutineType};
use auwsx_core::scheduler::{decide, Decision, Scheduler};
use auwsx_core::state::IssueStatus;
use auwsx_core::worktree::{
    branch_for_issue, issue_id_from_branch, orphaned_issue_worktrees, IssueWorktree,
    WorktreeHandle, Worktrees, WsxWorktrees,
};
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
        agent_summary: None,
        progress_report: None,
        result_report: None,
        status,
        branch: None,
        worktree_path: None,
        review_round: 0,
        conflict_attempts: 0,
        wait_until: None,
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
        profile_id: 1,
        profile_order: 1,
        name: "p".to_string(),
        repo_path: "/repo".to_string(),
        default_branch: "main".to_string(),
        arsenal_preset_name: None,
        main_agent_cmd: "main {prompt}".to_string(),
        route_agent_cmd: "main {prompt}".to_string(),
        plan_agent_cmd: "plan {prompt}".to_string(),
        work_agent_cmd: "work {prompt}".to_string(),
        review_agent_cmd: None,
        main_agent_cmd_override: Some("main {prompt}".to_string()),
        route_agent_cmd_override: Some("main {prompt}".to_string()),
        plan_agent_cmd_override: Some("plan {prompt}".to_string()),
        work_agent_cmd_override: Some("work {prompt}".to_string()),
        review_agent_cmd_override: None,
        completion_policy,
        completion_soft_timeout_min: 60,
        plan_gate_timeout_min: 10,
        iteration_timeout_min: 30,
        main_job_timeout_min: 60,
        review_max_rounds: 5,
        conflict_max_attempts: 3,
        max_concurrency,
        schedule_cron: None,
        merge_mode: MergeMode::Local,
        skill_path: None,
        deepsleep_cron: Some("0 0 * * 0".to_string()),
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
fn given_new_when_plan_phase_then_plan_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::New), Some((Role::Plan, true)));
}

#[test]
fn given_planning_when_plan_phase_then_plan_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::Planning), Some((Role::Plan, true)));
}

#[test]
fn given_working_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::Working), Some((Role::Work, true)));
}

#[test]
fn given_fixing_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::Fixing), Some((Role::Work, true)));
}

#[test]
fn given_audit_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::Auditing), Some((Role::Work, true)));
}

#[test]
fn given_resolving_conflict_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(
        prompt_plan(IssueStatus::ResolvingConflict),
        Some((Role::Work, true))
    );
}

#[test]
fn given_merging_when_plan_phase_then_work_needs_worktree() {
    assert_eq!(prompt_plan(IssueStatus::Merging), Some((Role::Work, true)));
}

#[test]
fn given_review_when_plan_phase_then_review_needs_worktree() {
    assert_eq!(
        prompt_plan(IssueStatus::Reviewing),
        Some((Role::Review, true))
    );
}

#[test]
fn given_each_human_gated_or_terminal_status_when_plan_phase_then_none() {
    for s in [
        IssueStatus::PlanReady,
        IssueStatus::PlanBlocked,
        IssueStatus::ReviewBlocked,
        IssueStatus::ConflictBlocked,
        IssueStatus::ReadyToMerge,
        IssueStatus::Done,
        IssueStatus::Abandoned,
        IssueStatus::Failed,
    ] {
        assert_eq!(prompt_plan(s), None, "{s:?} must not be actionable");
    }
}

#[test]
fn given_representative_statuses_when_plan_phase_then_some_iff_actionable() {
    // The Some-set is exactly the actionable set (pipeline.rs doc contract).
    for s in [
        IssueStatus::New,
        IssueStatus::Planning,
        IssueStatus::PlanReady,
        IssueStatus::Working,
        IssueStatus::Reviewing,
        IssueStatus::Fixing,
        IssueStatus::Auditing,
        IssueStatus::ResolvingConflict,
        IssueStatus::Merging,
        IssueStatus::ReadyToMerge,
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
    let issues = [issue_at(7, IssueStatus::Working)];
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::Spawn(7)]);
}

#[test]
fn given_actionable_issue_already_running_when_decide_then_no_decision() {
    let issues = [issue_at(7, IssueStatus::Working)];
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
        issue_at(1, IssueStatus::Working),
        issue_at(2, IssueStatus::Working),
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
        issue_at(1, IssueStatus::Working),
        issue_at(2, IssueStatus::Planning),
    ];
    let proj = project_with(2, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::Spawn(1), Decision::Spawn(2)]);
}

#[test]
fn given_local_merge_mode_and_many_merging_when_decide_then_spawns_one_merge() {
    let issues = [
        issue_at(1, IssueStatus::Merging),
        issue_at(2, IssueStatus::Merging),
        issue_at(3, IssueStatus::Working),
    ];
    let proj = project_with(3, CompletionPolicy::Auto);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::Spawn(1), Decision::Spawn(3)]);
}

#[test]
fn given_local_merge_already_running_when_decide_then_other_merge_waits() {
    let issues = [
        issue_at(1, IssueStatus::Merging),
        issue_at(2, IssueStatus::Merging),
        issue_at(3, IssueStatus::Working),
    ];
    let proj = project_with(3, CompletionPolicy::Auto);
    let mut running = HashSet::new();
    running.insert(1);
    let got = decide(&issues, &proj, &running, TS);
    assert_eq!(got, vec![Decision::Spawn(3)]);
}

#[test]
fn given_plan_ready_issue_with_no_wait_until_when_decide_then_soft_gate() {
    // PLAN_READY is always a soft gate and starts unarmed (wait_until None).
    let issues = [issue_at(3, IssueStatus::PlanReady)];
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::SoftGate(3)]);
}

#[test]
fn given_plan_ready_issue_armed_and_now_past_deadline_when_decide_then_soft_gate() {
    let mut issue = issue_at(3, IssueStatus::PlanReady);
    issue.wait_until = Some(TS); // deadline == now => due (now >= w)
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&[issue], &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::SoftGate(3)]);
}

#[test]
fn given_plan_ready_issue_armed_and_now_before_deadline_when_decide_then_no_decision() {
    let mut issue = issue_at(3, IssueStatus::PlanReady);
    issue.wait_until = Some(TS + 1); // not yet due
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&[issue], &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_ready_to_merge_issue_under_auto_policy_when_decide_then_soft_gate() {
    let issues = [issue_at(4, IssueStatus::ReadyToMerge)];
    let proj = project_with(1, CompletionPolicy::Auto);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::SoftGate(4)]);
}

#[test]
fn given_ready_to_merge_issue_under_soft_policy_unarmed_when_decide_then_soft_gate() {
    // wait_until None => needs arming, so it surfaces even under soft policy.
    let issues = [issue_at(4, IssueStatus::ReadyToMerge)];
    let proj = project_with(1, CompletionPolicy::Soft);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::SoftGate(4)]);
}

#[test]
fn given_ready_to_merge_issue_under_pr_merge_mode_when_decide_then_no_local_soft_gate() {
    let issues = [issue_at(4, IssueStatus::ReadyToMerge)];
    let mut proj = project_with(1, CompletionPolicy::Auto);
    proj.merge_mode = MergeMode::Pr;

    let got = decide(&issues, &proj, &empty_running(), TS);

    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_ready_to_merge_issue_under_manual_policy_when_decide_then_no_decision() {
    let issues = [issue_at(4, IssueStatus::ReadyToMerge)];
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, Vec::<Decision>::new());
}

#[test]
fn given_many_merging_local_issues_when_decide_then_oldest_merge_runs_first() {
    let issues = [
        issue_at(9, IssueStatus::Merging),
        issue_at(4, IssueStatus::Merging),
        issue_at(7, IssueStatus::Merging),
    ];
    let proj = project_with(3, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::Spawn(4)]);
}

#[test]
fn given_conflict_blocked_local_merge_when_decide_then_later_merges_wait() {
    let issues = [
        issue_at(9, IssueStatus::Merging),
        issue_at(4, IssueStatus::ConflictBlocked),
        issue_at(7, IssueStatus::Working),
    ];
    let proj = project_with(3, CompletionPolicy::Manual);
    let got = decide(&issues, &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::Spawn(7)]);
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
fn given_abandoned_issue_with_worktree_when_decide_then_teardown() {
    let mut issue = issue_at(8, IssueStatus::Abandoned);
    issue.worktree_path = Some("/wt".to_string());
    let proj = project_with(1, CompletionPolicy::Manual);
    let got = decide(&[issue], &proj, &empty_running(), TS);
    assert_eq!(got, vec![Decision::Teardown(8)]);
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
        pipeline_ux_guidance: None,
        memory_invocation: MemoryInvocation::Slash,
    };
    let text = prompt::build(&ctx).expect("actionable status yields a prompt");
    assert!(text.contains("42"), "prompt must name the issue id");
    assert!(
        text.contains("auwsx issue status"),
        "prompt must carry the control-CLI callback"
    );
    assert!(
        text.contains("before exiting, you must set exactly one"),
        "prompt must make status advancement a mandatory protocol"
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
        pipeline_ux_guidance: None,
        memory_invocation: MemoryInvocation::Slash,
    };
    assert!(
        prompt::build(&ctx).is_none(),
        "non-actionable status yields no prompt"
    );
}

#[test]
fn given_planning_issue_when_build_then_body_mentions_plan_ready_callback() {
    let issue = issue_at(1, IssueStatus::Planning);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
        pipeline_ux_guidance: None,
        memory_invocation: MemoryInvocation::Slash,
    };
    let text = prompt::build(&ctx).expect("planning prompt");
    assert!(
        text.contains("PLAN_READY"),
        "planning body must point at the PLAN_READY target"
    );
}

#[test]
fn given_review_issue_when_build_then_body_mentions_fixing_and_audit() {
    let issue = issue_at(1, IssueStatus::Reviewing);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
        pipeline_ux_guidance: None,
        memory_invocation: MemoryInvocation::Slash,
    };
    let text = prompt::build(&ctx).expect("review prompt");
    assert!(text.contains("FIXING"), "review body must offer FIXING");
    assert!(text.contains("AUDITING"), "review body must offer AUDITING");
}

#[test]
fn given_audit_issue_when_build_then_body_requests_human_verify_handoff() {
    let issue = issue_at(1, IssueStatus::Auditing);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
        pipeline_ux_guidance: None,
        memory_invocation: MemoryInvocation::Slash,
    };
    let text = prompt::build(&ctx).expect("audit prompt");
    assert!(
        text.contains(".auwsx/human-verify.md"),
        "audit prompt must leave stable human verification instructions"
    );
    assert!(
        text.contains("READY_TO_MERGE"),
        "audit prompt must still point at the merge gate"
    );
}

#[test]
fn given_pipeline_ux_guidance_when_build_then_header_includes_it() {
    let issue = issue_at(1, IssueStatus::Working);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
        pipeline_ux_guidance: Some("capability-driven UI, no duplicate paths"),
        memory_invocation: MemoryInvocation::Slash,
    };
    let text = prompt::build(&ctx).expect("working prompt");
    assert!(
        text.contains("capability-driven UI, no duplicate paths"),
        "worker prompts must include persisted auwsx guidance"
    );
    assert!(
        text.contains("cannot override system, developer, or repo instructions"),
        "worker prompts must bound persisted guidance authority"
    );
    assert!(
        text.contains("--- guidance start ---") && text.contains("--- guidance end ---"),
        "worker prompts must delimit persisted guidance"
    );
}

#[test]
fn given_blank_pipeline_ux_guidance_when_build_then_header_omits_guidance_block() {
    let issue = issue_at(1, IssueStatus::Working);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
        pipeline_ux_guidance: Some(" \n\t "),
        memory_invocation: MemoryInvocation::Slash,
    };

    let text = prompt::build(&ctx).expect("working prompt");

    assert!(!text.contains("Operator-configured auwsx guidance"));
    assert!(!text.contains("--- guidance start ---"));
}

#[test]
fn given_codex_memory_invocation_when_build_complete_then_uses_dollar_skill() {
    let issue = issue_at(1, IssueStatus::Merging);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
        pipeline_ux_guidance: None,
        memory_invocation: MemoryInvocation::Dollar,
    };

    let text = prompt::build(&ctx).expect("merge prompt");

    assert!(text.contains("$memory-save"));
    assert!(!text.contains("/memo"));
}

#[test]
fn given_merge_prompt_when_built_then_includes_dirty_main_recovery_contract() {
    let issue = issue_at(1, IssueStatus::Merging);
    let ctx = PromptContext {
        issue: &issue,
        subtasks: &[],
        steering: &[],
        open_findings: &[],
        pipeline_ux_guidance: None,
        memory_invocation: MemoryInvocation::Dollar,
    };

    let text = prompt::build(&ctx).expect("merge prompt");

    assert!(text.contains("\"$AUWSX_BIN\" issue apply-merge \"$AUWSX_ISSUE_ID\""));
    assert!(text.contains("Do not hand-roll primary-worktree stash/merge/restore commands"));
    assert!(text.contains("CONFLICT_BLOCKED"));
    assert!(text.contains("set exactly one issue status"));
    assert!(text.contains("do not invoke"));
    assert!(text.contains("$memory-save"));
    assert!(text.contains("again"));
    assert!(text.contains("issue-local control mode"));
    assert!(text.contains("`apply-merge` owns the final status transition"));
}

#[test]
fn given_prompt_preview_count_when_checked_then_matches_catalog_len() {
    assert_eq!(prompt::preview_count(), prompt::preview_catalog().len());
}

#[test]
fn given_prompt_catalog_when_reviewed_then_includes_each_spawned_phase() {
    let statuses: Vec<IssueStatus> = prompt::preview_catalog()
        .into_iter()
        .map(|preview| preview.status)
        .collect();
    assert_eq!(
        statuses,
        vec![
            IssueStatus::Planning,
            IssueStatus::Working,
            IssueStatus::Reviewing,
            IssueStatus::Fixing,
            IssueStatus::Auditing,
            IssueStatus::ResolvingConflict,
            IssueStatus::Merging,
        ]
    );
}

#[test]
fn given_prompt_catalog_when_reviewed_then_each_prompt_has_control_contract() {
    for preview in prompt::preview_catalog() {
        assert!(
            preview.text.contains("Control CLI"),
            "{} prompt must include control CLI section",
            preview.status.as_str()
        );
        assert!(
            preview
                .text
                .contains("\"$AUWSX_BIN\" issue status \"$AUWSX_ISSUE_ID\""),
            "{} prompt must include status callback form",
            preview.status.as_str()
        );
    }
}

#[test]
fn given_prompt_catalog_when_reviewed_then_issue_workers_stay_bounded() {
    for preview in prompt::preview_catalog() {
        assert!(
            preview.text.contains("do not spawn subagents"),
            "{} prompt must forbid delegated workers",
            preview.status.as_str()
        );
        assert!(
            !preview.text.contains("/good-to-go") && !preview.text.contains("/backpressure"),
            "{} prompt must not depend on slash-skill workflows",
            preview.status.as_str()
        );
        assert!(
            preview.text.contains("/no-repeat"),
            "{} prompt must include repeat-failure breadcrumb guidance",
            preview.status.as_str()
        );
    }
}

// ===========================================================================
// worktree::branch_for_issue
// ===========================================================================

#[test]
fn given_issue_id_when_branch_for_issue_then_matches_template() {
    assert_eq!(branch_for_issue(17), "auwsx/issue-17");
}

#[test]
fn given_auwsx_issue_branch_when_parsed_then_issue_id_returned() {
    assert_eq!(issue_id_from_branch("auwsx/issue-17"), Some(17));
}

#[test]
fn given_non_issue_branch_when_parsed_then_none() {
    assert_eq!(issue_id_from_branch("feature/issue-17"), None);
}

#[test]
fn given_malformed_issue_branch_when_parsed_then_none() {
    assert_eq!(issue_id_from_branch("auwsx/issue-"), None);
}

#[test]
fn given_issue_worktree_missing_from_known_paths_when_orphaned_then_selected() {
    let worktrees = vec![IssueWorktree {
        issue_id: 3,
        handle: WorktreeHandle {
            branch: branch_for_issue(3),
            path: PathBuf::from("/repo-auwsx-issue-3"),
        },
    }];

    assert_eq!(
        orphaned_issue_worktrees(&worktrees, &HashMap::new()),
        worktrees
    );
}

#[test]
fn given_issue_worktree_with_matching_known_path_when_orphaned_then_kept() {
    let path = PathBuf::from("/repo-auwsx-issue-3");
    let worktrees = vec![IssueWorktree {
        issue_id: 3,
        handle: WorktreeHandle {
            branch: branch_for_issue(3),
            path: path.clone(),
        },
    }];
    let mut known = HashMap::new();
    known.insert(3, path);

    assert!(orphaned_issue_worktrees(&worktrees, &known).is_empty());
}

#[test]
fn given_issue_worktree_with_different_known_path_when_orphaned_then_selected() {
    let worktrees = vec![IssueWorktree {
        issue_id: 3,
        handle: WorktreeHandle {
            branch: branch_for_issue(3),
            path: PathBuf::from("/repo-auwsx-issue-3"),
        },
    }];
    let mut known = HashMap::new();
    known.insert(3, PathBuf::from("/other"));

    assert_eq!(orphaned_issue_worktrees(&worktrees, &known), worktrees);
}

fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git -C {} {} failed\nstdout:\n{}\nstderr:\n{}",
            repo.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn init_git_repo(repo: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(repo)?;
    git(repo, &["init"])?;
    git(
        repo,
        &["config", "user.email", "auwsx-test@example.invalid"],
    )?;
    git(repo, &["config", "user.name", "auwsx test"])?;
    std::fs::write(repo.join("README.md"), "seed\n")?;
    git(repo, &["add", "README.md"])?;
    git(repo, &["commit", "-m", "seed"])?;
    git(repo, &["branch", "-M", "main"])?;
    Ok(())
}

#[tokio::test]
async fn given_prunable_issue_worktree_branch_when_create_then_archives_and_creates(
) -> anyhow::Result<()> {
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("stale-worktree-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let stale_path = tmp.path().join("stale-issue-2");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "auwsx/issue-2",
            stale_path.to_str().expect("utf8 test path"),
            "main",
        ],
    )?;
    std::fs::remove_dir_all(&stale_path)?;
    let mut project = project_with(1, CompletionPolicy::Manual);
    project.repo_path = repo.display().to_string();
    project.default_branch = "main".to_string();

    let handle = WsxWorktrees.create(&project, "auwsx/issue-2").await?;

    assert_eq!(handle.branch, "auwsx/issue-2");
    assert!(handle.path.exists(), "new worktree must exist");
    let branches = git(&repo, &["branch", "--list", "auwsx/*"])?;
    assert!(
        branches.contains("auwsx/orphaned/issue-2"),
        "old colliding branch should be preserved under orphan namespace:\n{branches}"
    );
    assert!(
        branches.contains("auwsx/issue-2"),
        "requested issue branch should be recreated:\n{branches}"
    );
    Ok(())
}

#[tokio::test]
async fn given_live_issue_worktree_branch_when_create_then_refuses_overwrite() -> anyhow::Result<()>
{
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("live-worktree-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let live_path = tmp.path().join("live-issue-2");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "auwsx/issue-2",
            live_path.to_str().expect("utf8 test path"),
            "main",
        ],
    )?;
    let mut project = project_with(1, CompletionPolicy::Manual);
    project.repo_path = repo.display().to_string();
    project.default_branch = "main".to_string();

    let err = WsxWorktrees
        .create(&project, "auwsx/issue-2")
        .await
        .expect_err("live worktree collision must be refused");

    assert!(
        err.to_string().contains("already checked out"),
        "unexpected error: {err:#}"
    );
    assert!(live_path.exists(), "live worktree must not be removed");
    let branches = git(&repo, &["branch", "--list", "auwsx/*"])?;
    assert!(
        !branches.contains("auwsx/orphaned/issue-2"),
        "live branch must not be archived:\n{branches}"
    );
    Ok(())
}

// ===========================================================================
// The DRIVE test: the Scheduler carries one issue NEW -> DONE using a
// fake agent (status transitions only) and a temp-dir worktree (no git).
//
// Pipeline-owned edge:  NEW->PLANNING before the planner starts.
// Agent-driven edges:   PLANNING->PLAN_READY, WORKING->REVIEW,
//                       REVIEW->AUDITING, AUDITING->READY_TO_MERGE, MERGING->DONE.
// Scheduler soft-gate:  PLAN_READY->WORKING, READY_TO_MERGE->MERGING.
// Human loop-back:      READY_TO_MERGE->WORKING.
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

struct FailingWorktrees;
#[async_trait]
impl Worktrees for FailingWorktrees {
    async fn create(&self, _p: &Project, _branch: &str) -> anyhow::Result<WorktreeHandle> {
        anyhow::bail!("worktree branch collision")
    }
    async fn teardown(&self, _p: &Project, _h: &WorktreeHandle) -> anyhow::Result<()> {
        Ok(())
    }
}

struct FailingTeardownWorktrees;
#[async_trait]
impl Worktrees for FailingTeardownWorktrees {
    async fn create(&self, _p: &Project, branch: &str) -> anyhow::Result<WorktreeHandle> {
        Ok(WorktreeHandle {
            branch: branch.to_string(),
            path: PathBuf::from("/tmp/unused"),
        })
    }

    async fn teardown(&self, _p: &Project, _h: &WorktreeHandle) -> anyhow::Result<()> {
        anyhow::bail!("teardown failed")
    }
}

struct BlockingTeardownWorktrees {
    started: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl Worktrees for BlockingTeardownWorktrees {
    async fn create(&self, _p: &Project, branch: &str) -> anyhow::Result<WorktreeHandle> {
        Ok(WorktreeHandle {
            branch: branch.to_string(),
            path: PathBuf::from("/tmp/unused"),
        })
    }

    async fn teardown(&self, _p: &Project, _h: &WorktreeHandle) -> anyhow::Result<()> {
        self.started.notify_waiters();
        self.release.notified().await;
        Ok(())
    }
}

/// Reads `AUWSX_ISSUE_ID` from the spec env, applies the single transition a
/// real agent would request via the control CLI for the current phase, then
/// "exits" cleanly. PLAN_READY->WORKING and READY_TO_MERGE->MERGING are
/// scheduler soft gates; READY_TO_MERGE->WORKING is a human loop-back.
struct ScriptedAgent {
    db: Db,
    now: i64,
}
#[async_trait]
impl AgentExecutor for ScriptedAgent {
    async fn execute(&self, spec: AgentSpec<'_>) -> anyhow::Result<AgentOutcome> {
        let Some((_, issue_raw)) = spec.env.iter().find(|(k, _)| k == "AUWSX_ISSUE_ID") else {
            return Ok(AgentOutcome {
                exit_kind: ExitKind::Exited,
                exit_code: Some(0),
                pid: None,
            });
        };
        let issue_id: i64 = issue_raw.parse().expect("issue id parses");
        let outbox = spec
            .env
            .iter()
            .find(|(k, _)| k == control_outbox::OUTBOX_ENV)
            .expect("pipeline injects AUWSX_CONTROL_OUTBOX")
            .1
            .clone();
        assert!(
            spec.env.iter().all(|(k, _)| k != "AUWSX_SOCK"),
            "issue workers must not receive unrestricted daemon socket access"
        );
        assert!(
            Path::new(&outbox).parent().is_some_and(Path::exists),
            "pipeline prepares the control outbox directory during agent execution"
        );
        let cur = issues::get(self.db.pool(), issue_id)
            .await?
            .expect("issue exists during run")
            .status;
        let report_dir = spec.cwd.join(".auwsx");
        std::fs::create_dir_all(&report_dir)?;
        std::fs::write(
            report_dir.join("phase-report.md"),
            format!(
                "Phase {} completed by scripted agent.\nVerified transition behavior in test.",
                cur.as_str()
            ),
        )?;
        let next = match cur {
            IssueStatus::Planning => Some(IssueStatus::PlanReady),
            IssueStatus::Working => Some(IssueStatus::Reviewing),
            IssueStatus::Reviewing => Some(IssueStatus::Auditing), // clean review, no findings
            IssueStatus::Auditing => Some(IssueStatus::ReadyToMerge),
            IssueStatus::Merging => Some(IssueStatus::Done),
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

#[derive(Default)]
struct TestRemoteExecutor {
    seen: Arc<Mutex<Vec<RemoteSyncKind>>>,
    observed_state: Option<RemotePrState>,
}

#[async_trait]
impl RemoteProviderExecutor for TestRemoteExecutor {
    async fn execute(&self, request: RemoteSyncRequest) -> anyhow::Result<RemoteProviderEffect> {
        self.seen.lock().unwrap().push(request.run.kind);
        match request.action {
            RemotePlannedAction::CreateIssue { .. } => {
                Ok(RemoteProviderEffect::Issue(CreatedRemoteIssue {
                    number: 177,
                    node_id: None,
                    url: "https://github.com/acme/app/issues/177".to_string(),
                }))
            }
            RemotePlannedAction::CreateOrUpdatePullRequest {
                head_branch,
                base_branch,
                ..
            } => Ok(RemoteProviderEffect::PullRequest(CreatedRemotePr {
                number: request.issue.id,
                node_id: None,
                url: format!("https://github.com/acme/app/pull/{}", request.issue.id),
                head_branch,
                head_sha: None,
                base_branch,
                base_sha: None,
                state: auwsx_core::db::remote::RemotePrState::Open,
                check_status: RemotePrCheckStatus::Unknown,
                check_summary: None,
                merge_state_status: None,
                review_decision: None,
            })),
            RemotePlannedAction::PostProgressComment { .. } => Ok(RemoteProviderEffect::Comment),
        }
    }

    async fn observe_pull_request(
        &self,
        _config: &ProjectRemoteConfig,
        pr_link: &RemotePrLink,
    ) -> anyhow::Result<CreatedRemotePr> {
        Ok(CreatedRemotePr {
            number: pr_link.remote_pr_number,
            node_id: pr_link.remote_node_id.clone(),
            url: pr_link.remote_url.clone(),
            head_branch: pr_link.head_branch.clone(),
            head_sha: pr_link.head_sha.clone(),
            base_branch: pr_link.base_branch.clone(),
            base_sha: pr_link.base_sha.clone(),
            state: self.observed_state.unwrap_or(pr_link.state),
            check_status: pr_link.check_status,
            check_summary: pr_link.check_summary.clone(),
            merge_state_status: pr_link.merge_state_status.clone(),
            review_decision: pr_link.review_decision.clone(),
        })
    }
}

struct RecordingAgent {
    cmd_template: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl AgentExecutor for RecordingAgent {
    async fn execute(&self, spec: AgentSpec<'_>) -> anyhow::Result<AgentOutcome> {
        *self.cmd_template.lock().expect("recording lock") = Some(spec.cmd_template.to_string());
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

struct NonzeroNoopAgent;
#[async_trait]
impl AgentExecutor for NonzeroNoopAgent {
    async fn execute(&self, _spec: AgentSpec<'_>) -> anyhow::Result<AgentOutcome> {
        Ok(AgentOutcome {
            exit_kind: ExitKind::Exited,
            exit_code: Some(2),
            pid: None,
        })
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
    drive_project_with_repo(pool, "/repo").await
}

async fn drive_project_with_repo(pool: &SqlitePool, repo_path: &str) -> anyhow::Result<i64> {
    let id = projects::create(
        pool,
        NewProject {
            name: "drive",
            repo_path,
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "main {prompt}",
            route_agent_cmd: "main {prompt}",
            plan_agent_cmd: "plan {prompt}",
            work_agent_cmd: "work {prompt}",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Auto),
            plan_gate_timeout_min: Some(0),
            completion_soft_timeout_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    Ok(id)
}

async fn enable_project_remote(pool: &SqlitePool, project_id: i64) -> anyhow::Result<()> {
    remote::upsert_config(
        pool,
        UpsertProjectRemoteConfig {
            project_id,
            provider: RemoteProvider::Github,
            remote_url: "https://github.com/acme/app",
            owner: "acme",
            repo: "app",
            api_base_url: "https://api.github.com",
            auth_kind: RemoteAuthKind::None,
            auth_ref: None,
            webhook_secret_ref: None,
            inbound_auwsx_run_enabled: true,
            outbound_issue_create_enabled: true,
            remote_pr_merge_enabled: true,
            agent_comment_sync_enabled: false,
            subtask_comment_sync_enabled: false,
            finding_comment_sync_enabled: false,
            draft_pr_enabled: false,
            required_checks_policy: RequiredChecksPolicy::Observe,
            default_labels: None,
            default_assignees: None,
            pr_base_branch: Some("main"),
        },
        TS,
    )
    .await?;
    Ok(())
}

async fn attach_clean_issue_branch(
    pool: &SqlitePool,
    repo: &Path,
    issue_id: i64,
    file_name: &str,
) -> anyhow::Result<()> {
    let branch = branch_for_issue(issue_id);
    git(repo, &["checkout", "-b", &branch])?;
    std::fs::write(repo.join(file_name), format!("issue {issue_id}\n"))?;
    git(repo, &["add", file_name])?;
    git(repo, &["commit", "-m", &format!("issue {issue_id}")])?;
    git(repo, &["checkout", "main"])?;
    issues::set_worktree(pool, issue_id, Some(&branch), None, TS).await?;
    Ok(())
}

async fn attach_conflicting_issue_branch(
    pool: &SqlitePool,
    repo: &Path,
    issue_id: i64,
) -> anyhow::Result<()> {
    let branch = branch_for_issue(issue_id);
    git(repo, &["checkout", "-b", &branch])?;
    std::fs::write(repo.join("README.md"), format!("issue {issue_id}\n"))?;
    git(repo, &["add", "README.md"])?;
    git(repo, &["commit", "-m", &format!("issue {issue_id}")])?;
    git(repo, &["checkout", "main"])?;
    std::fs::write(repo.join("README.md"), "main change\n")?;
    git(repo, &["add", "README.md"])?;
    git(repo, &["commit", "-m", "main change"])?;
    issues::set_worktree(pool, issue_id, Some(&branch), None, TS).await?;
    Ok(())
}

#[tokio::test]
async fn given_control_dir_placeholder_when_issue_runs_then_uses_issue_control_dir(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    std::fs::create_dir_all(".tmp")?;
    let data_tmp = tempfile::Builder::new()
        .prefix("auwsx-data-")
        .tempdir_in(".tmp")?;
    let worktree_tmp = tempfile::Builder::new()
        .prefix("auwsx-worktree-")
        .tempdir_in(".tmp")?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    sqlx::query("UPDATE projects SET plan_agent_cmd = ? WHERE id = ?")
        .bind("plan --control-dir {auwsx_control_dir} --socket-dir {auwsx_socket_dir} {prompt}")
        .bind(project_id)
        .execute(db.pool())
        .await?;
    let issue_id = issues::create(db.pool(), project_id, "placeholder", None, TS).await?;
    let recorded = Arc::new(Mutex::new(None));
    let agent = RecordingAgent {
        cmd_template: recorded.clone(),
    };
    let clock = FixedClock(TS);
    let worktrees = FakeWorktrees(worktree_tmp.path().to_path_buf());
    let events = events::channel();
    let deps = auwsx_core::pipeline::Deps {
        db: &db,
        clock: &clock,
        executor: &agent,
        worktrees: &worktrees,
        events: &events,
        socket: PathBuf::from("/daemon/cache/auwsx.sock"),
    };

    auwsx_core::pipeline::execute(&deps, issue_id).await?;

    let cmd = recorded.lock().expect("recording lock").clone().unwrap();
    assert!(cmd.contains(&format!(
        "--control-dir {}",
        worktree_tmp.path().join(".auwsx/control").display()
    )));
    assert!(cmd.contains("--socket-dir /daemon/cache"));
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_worktree_create_failure_when_issue_runs_then_setup_error_is_logged(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    std::fs::create_dir_all(".tmp")?;
    let data_tmp = tempfile::Builder::new()
        .prefix("auwsx-data-")
        .tempdir_in(".tmp")?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "fails before spawn", None, TS).await?;
    let events = events::channel();
    let deps = auwsx_core::pipeline::Deps {
        db: &db,
        clock: &FixedClock(TS + 1),
        executor: &ExitAgent,
        worktrees: &FailingWorktrees,
        events: &events,
        socket: data_tmp.path().join("daemon.sock"),
    };

    let err = auwsx_core::pipeline::execute(&deps, issue_id)
        .await
        .expect_err("worktree create failure must propagate");

    assert!(
        err.to_string().contains("creating worktree"),
        "unexpected error: {err:#}"
    );
    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    assert_eq!(issue.status, IssueStatus::Failed);
    let runs = agent_runs::list_by_issue(db.pool(), issue_id).await?;
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.exit_kind, Some(ExitKind::Error));
    assert_eq!(
        run.status_after.as_deref(),
        Some(IssueStatus::Failed.as_str())
    );
    assert!(
        run.note
            .as_deref()
            .is_some_and(|note| note.contains("worktree branch collision")),
        "setup failure note must include cause: {:?}",
        run.note
    );
    let log_path = run.log_path.as_ref().expect("setup failure log path");
    let log = std::fs::read_to_string(log_path)?;
    assert!(log.contains("\"kind\":\"setup_error\""), "{log}");
    assert!(log.contains("worktree branch collision"), "{log}");
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

async fn set_project_runtime_policy(
    pool: &SqlitePool,
    project_id: i64,
    max_concurrency: i64,
    schedule_cron: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE projects
         SET max_concurrency = ?, schedule_cron = ?
         WHERE id = ?",
    )
    .bind(max_concurrency)
    .bind(schedule_cron)
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn set_project_deepsleep_interval(
    pool: &SqlitePool,
    project_id: i64,
    interval_days: i64,
    last_ran_at: Option<i64>,
) -> anyhow::Result<()> {
    let deepsleep_cron = match interval_days {
        days if days <= 0 => None,
        1 => Some("0 0 * * *".to_string()),
        7 => Some("0 0 * * 0".to_string()),
        days => Some(format!("0 0 */{days} * *")),
    };
    sqlx::query(
        "UPDATE projects
         SET deepsleep_cron = ?, last_deepsleep_at = ?
         WHERE id = ?",
    )
    .bind(deepsleep_cron)
    .bind(last_ran_at)
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn finished_reconcile_job_with_log(
    pool: &SqlitePool,
    project_id: i64,
    log: impl AsRef<str>,
) -> anyhow::Result<(i64, PathBuf)> {
    let main_job_id = main_jobs::enqueue_project_reconcile(pool, project_id, "prompt", TS).await?;
    let log_path = artifacts::main_job_log_path(project_id, main_job_id, TS + 1)?;
    std::fs::write(&log_path, log.as_ref())?;
    main_jobs::mark_running(pool, main_job_id, TS + 1, &log_path.to_string_lossy()).await?;
    main_jobs::finish(pool, main_job_id, MainJobStatus::Done, TS + 2, None).await?;
    Ok((main_job_id, log_path))
}

fn scheduler_with(
    db: Db,
    clock: Arc<dyn Clock>,
    executor: Arc<dyn AgentExecutor>,
    tick_interval: Duration,
) -> Scheduler {
    scheduler_with_worktrees(
        db,
        clock,
        executor,
        Arc::new(FakeWorktrees(PathBuf::from("/tmp/auwsx-test-worktree"))),
        tick_interval,
    )
}

fn scheduler_with_worktrees(
    db: Db,
    clock: Arc<dyn Clock>,
    executor: Arc<dyn AgentExecutor>,
    worktrees: Arc<dyn Worktrees>,
    tick_interval: Duration,
) -> Scheduler {
    Scheduler::new(
        db,
        clock,
        executor,
        worktrees,
        events::channel(),
        PathBuf::from("/tmp/unused.sock"),
        tick_interval,
    )
}

fn scheduler_with_remote_executor(
    db: Db,
    clock: Arc<dyn Clock>,
    executor: Arc<dyn AgentExecutor>,
    remote_executor: Arc<dyn RemoteProviderExecutor>,
    tick_interval: Duration,
) -> Scheduler {
    Scheduler::new_with_remote_executor(
        db,
        clock,
        executor,
        remote_executor,
        Arc::new(FakeWorktrees(PathBuf::from("/tmp/auwsx-test-worktree"))),
        events::channel(),
        PathBuf::from("/tmp/unused.sock"),
        tick_interval,
    )
}

#[tokio::test]
async fn given_ready_project_issues_when_project_merge_approved_then_all_release_oldest_first(
) -> anyhow::Result<()> {
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("project-merge-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let db = Db::open_memory().await?;
    let project_id = drive_project_with_repo(db.pool(), &repo.display().to_string()).await?;
    let first = issues::create(db.pool(), project_id, "first ready", None, TS).await?;
    let second = issues::create(db.pool(), project_id, "second ready", None, TS).await?;
    attach_clean_issue_branch(db.pool(), &repo, first, "first.txt").await?;
    attach_clean_issue_branch(db.pool(), &repo, second, "second.txt").await?;
    issues::force_status(db.pool(), first, IssueStatus::ReadyToMerge, TS).await?;
    issues::force_status(db.pool(), second, IssueStatus::ReadyToMerge, TS).await?;
    let release = Arc::new(Notify::new());
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(BlockingAgent {
            started: Arc::new(Notify::new()),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );

    let released = scheduler.approve_project_merge(project_id, TS).await?;

    assert_eq!(released, vec![first, second]);
    assert_eq!(
        issues::get(db.pool(), first)
            .await?
            .expect("first issue exists")
            .status,
        IssueStatus::Merging
    );
    assert_eq!(
        issues::get(db.pool(), second)
            .await?
            .expect("second issue exists")
            .status,
        IssueStatus::Merging
    );
    release.notify_waiters();
    Ok(())
}

#[tokio::test]
async fn given_pr_merge_project_when_issue_merge_approved_then_remote_pr_sync_is_queued(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    sqlx::query("UPDATE projects SET merge_mode = 'pr' WHERE id = ?")
        .bind(project_id)
        .execute(db.pool())
        .await?;
    enable_project_remote(db.pool(), project_id).await?;
    let issue_id = issues::create(db.pool(), project_id, "ready remote pr", None, TS).await?;
    issues::set_worktree(
        db.pool(),
        issue_id,
        Some("auwsx/issue-remote-pr"),
        Some("/worktree"),
        TS,
    )
    .await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let remote_executor = Arc::new(TestRemoteExecutor::default());
    let scheduler = scheduler_with_remote_executor(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        remote_executor,
        Duration::from_secs(60),
    );

    let released = scheduler.approve_issue_merge(issue_id, TS).await?;
    scheduler.tick_project(project_id).await?;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    let runs = remote::recent_sync_runs(db.pool(), project_id, 10).await?;
    let pr_link = remote::pr_link_by_issue(db.pool(), issue_id)
        .await?
        .expect("PR link exists");
    assert_eq!(
        (released, issue.status, pr_link.remote_pr_number),
        (vec![issue_id], IssueStatus::ReadyToMerge, issue_id)
    );
    assert!(runs
        .iter()
        .any(|run| run.kind == RemoteSyncKind::Pr && run.status == RemoteSyncStatus::Done));
    Ok(())
}

#[tokio::test]
async fn given_pr_merge_project_when_remote_pr_observed_merged_then_issue_done(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    sqlx::query("UPDATE projects SET merge_mode = 'pr' WHERE id = ?")
        .bind(project_id)
        .execute(db.pool())
        .await?;
    enable_project_remote(db.pool(), project_id).await?;
    let issue_id = issues::create(db.pool(), project_id, "remote merged pr", None, TS).await?;
    issues::set_worktree(
        db.pool(),
        issue_id,
        Some("auwsx/issue-remote-pr"),
        Some("/worktree"),
        TS,
    )
    .await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let pr_link_id = remote::upsert_pr_link(
        db.pool(),
        UpsertRemotePrLink {
            project_id,
            issue_id,
            provider: RemoteProvider::Github,
            remote_owner: "acme",
            remote_repo: "app",
            remote_pr_number: 44,
            remote_node_id: None,
            remote_url: "https://github.com/acme/app/pull/44",
            head_branch: "auwsx/issue-remote-pr",
            head_sha: None,
            base_branch: "main",
            base_sha: None,
            state: RemotePrState::Open,
            check_status: RemotePrCheckStatus::Unknown,
            check_summary: None,
            merge_state_status: None,
            review_decision: None,
            last_synced_at: Some(TS),
        },
        TS,
    )
    .await?;
    let remote_executor = Arc::new(TestRemoteExecutor {
        seen: Arc::new(Mutex::new(Vec::new())),
        observed_state: Some(RemotePrState::Merged),
    });
    let scheduler = scheduler_with_remote_executor(
        db.clone(),
        Arc::new(FixedClock(TS + 1)),
        Arc::new(ExitAgent),
        remote_executor,
        Duration::from_secs(60),
    );

    scheduler.tick_project(project_id).await?;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    let pr_link = remote::pr_link_by_issue(db.pool(), issue_id)
        .await?
        .expect("PR link exists");
    let runs = remote::recent_sync_runs(db.pool(), project_id, 10).await?;
    assert_eq!(issue.status, IssueStatus::Done);
    assert_eq!(pr_link.state, RemotePrState::Merged);
    assert!(issue
        .result_report
        .as_deref()
        .is_some_and(|report| report.contains("Remote PR merged")));
    assert!(runs.iter().any(|run| {
        run.direction == RemoteSyncDirection::Inbound
            && run.kind == RemoteSyncKind::Pr
            && run.status == RemoteSyncStatus::Done
            && run.remote_pr_link_id == Some(pr_link_id)
    }));
    Ok(())
}

#[tokio::test]
async fn given_pr_merge_project_when_project_merge_approved_then_all_ready_pr_syncs_are_queued(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    sqlx::query("UPDATE projects SET merge_mode = 'pr' WHERE id = ?")
        .bind(project_id)
        .execute(db.pool())
        .await?;
    enable_project_remote(db.pool(), project_id).await?;
    let first = issues::create(db.pool(), project_id, "first remote pr", None, TS).await?;
    let second = issues::create(db.pool(), project_id, "second remote pr", None, TS).await?;
    for (issue_id, branch) in [(first, "auwsx/issue-first"), (second, "auwsx/issue-second")] {
        issues::set_worktree(db.pool(), issue_id, Some(branch), Some("/worktree"), TS).await?;
        issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    }
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let released = scheduler.approve_project_merge(project_id, TS).await?;

    let runs = remote::recent_sync_runs(db.pool(), project_id, 10).await?;
    let pr_runs = runs
        .iter()
        .filter(|run| run.kind == RemoteSyncKind::Pr)
        .count();
    assert_eq!((released, pr_runs), (vec![first, second], 2));
    Ok(())
}

#[tokio::test]
async fn given_conflict_blocked_local_merge_when_project_merge_approved_then_ready_items_stay_queued(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let blocked = issues::create(db.pool(), project_id, "blocked", None, TS).await?;
    let ready = issues::create(db.pool(), project_id, "ready", None, TS).await?;
    issues::force_status(db.pool(), blocked, IssueStatus::ConflictBlocked, TS).await?;
    issues::force_status(db.pool(), ready, IssueStatus::ReadyToMerge, TS).await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let err = scheduler
        .approve_project_merge(project_id, TS)
        .await
        .expect_err("conflict-blocked local merge must gate later releases");
    let ready_issue = issues::get(db.pool(), ready)
        .await?
        .expect("ready issue exists");

    assert!(
        err.to_string().contains("conflict-blocked issue"),
        "unexpected error: {err:#}"
    );
    assert_eq!(ready_issue.status, IssueStatus::ReadyToMerge);
    Ok(())
}

#[tokio::test]
async fn given_represented_ready_issue_when_reconcile_project_then_marks_done() -> anyhow::Result<()>
{
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("reconcile-represented-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let db = Db::open_memory().await?;
    let project_id = drive_project_with_repo(db.pool(), &repo.display().to_string()).await?;
    let issue_id = issues::create(db.pool(), project_id, "represented", None, TS).await?;
    attach_clean_issue_branch(db.pool(), &repo, issue_id, "represented.txt").await?;
    git(
        &repo,
        &[
            "merge",
            "--no-ff",
            &branch_for_issue(issue_id),
            "-m",
            "represented",
        ],
    )?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let report = scheduler.reconcile_project(project_id, TS + 1).await?;

    assert_eq!(report.applied_count, 1);
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::Done
    );
    Ok(())
}

#[tokio::test]
async fn given_represented_ready_issue_when_cleanup_fails_then_mark_done_claim_happens_first(
) -> anyhow::Result<()> {
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("reconcile-markdone-order-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let db = Db::open_memory().await?;
    let project_id = drive_project_with_repo(db.pool(), &repo.display().to_string()).await?;
    let issue_id = issues::create(db.pool(), project_id, "represented", None, TS).await?;
    attach_clean_issue_branch(db.pool(), &repo, issue_id, "represented-order.txt").await?;
    issues::set_worktree(
        db.pool(),
        issue_id,
        Some(&branch_for_issue(issue_id)),
        Some("/tmp/auwsx-represented-order"),
        TS,
    )
    .await?;
    git(
        &repo,
        &[
            "merge",
            "--no-ff",
            &branch_for_issue(issue_id),
            "-m",
            "represented",
        ],
    )?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let scheduler = scheduler_with_worktrees(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Arc::new(FailingTeardownWorktrees),
        Duration::from_secs(60),
    );

    let err = scheduler
        .reconcile_project(project_id, TS + 1)
        .await
        .expect_err("teardown failure should surface after DB claim");
    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");

    assert!(
        err.to_string().contains("teardown failed"),
        "unexpected error: {err:#}"
    );
    assert_eq!(issue.status, IssueStatus::Done);
    assert_eq!(
        issue.worktree_path.as_deref(),
        Some("/tmp/auwsx-represented-order")
    );
    Ok(())
}

#[tokio::test]
async fn given_two_clean_ready_issues_when_reconcile_project_then_releases_one_merge(
) -> anyhow::Result<()> {
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("reconcile-one-merge-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let db = Db::open_memory().await?;
    let project_id = drive_project_with_repo(db.pool(), &repo.display().to_string()).await?;
    let first = issues::create(db.pool(), project_id, "first", None, TS).await?;
    let second = issues::create(db.pool(), project_id, "second", None, TS).await?;
    attach_clean_issue_branch(db.pool(), &repo, first, "first.txt").await?;
    attach_clean_issue_branch(db.pool(), &repo, second, "second.txt").await?;
    issues::force_status(db.pool(), first, IssueStatus::ReadyToMerge, TS).await?;
    issues::force_status(db.pool(), second, IssueStatus::ReadyToMerge, TS).await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let report = scheduler.reconcile_project(project_id, TS + 1).await?;
    let first_status = issues::get(db.pool(), first).await?.expect("first").status;
    let second_status = issues::get(db.pool(), second)
        .await?
        .expect("second")
        .status;

    assert_eq!(report.applied_count, 1);
    assert_eq!(
        [first_status, second_status]
            .into_iter()
            .filter(|status| *status == IssueStatus::Merging)
            .count(),
        1
    );
    assert_eq!(
        [first_status, second_status]
            .into_iter()
            .filter(|status| *status == IssueStatus::ReadyToMerge)
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn given_reconcile_dry_run_when_clean_issue_exists_then_no_mutation_or_job(
) -> anyhow::Result<()> {
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("reconcile-dry-run-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let db = Db::open_memory().await?;
    let project_id = drive_project_with_repo(db.pool(), &repo.display().to_string()).await?;
    let issue_id = issues::create(db.pool(), project_id, "dry run", None, TS).await?;
    attach_clean_issue_branch(db.pool(), &repo, issue_id, "dry-run.txt").await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let report = scheduler.diagnose_project(project_id, true).await?;

    assert_eq!(report.applied_count, 0);
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::ReadyToMerge
    );
    assert!(main_jobs::recent_by_project(db.pool(), project_id, 10)
        .await?
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn given_conflicting_ready_issue_when_reconcile_twice_then_one_active_reconcile_job(
) -> anyhow::Result<()> {
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("reconcile-agentic-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let db = Db::open_memory().await?;
    let project_id = drive_project_with_repo(db.pool(), &repo.display().to_string()).await?;
    let issue_id = issues::create(db.pool(), project_id, "conflict", None, TS).await?;
    attach_conflicting_issue_branch(db.pool(), &repo, issue_id).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(BlockingAgent {
            started: started.clone(),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );

    scheduler.reconcile_project(project_id, TS + 1).await?;
    started.notified().await;
    scheduler.reconcile_project(project_id, TS + 2).await?;

    let jobs = main_jobs::recent_by_project(db.pool(), project_id, 10).await?;
    assert_eq!(
        jobs.iter()
            .filter(|job| job.kind == "reconcile"
                && matches!(job.status, MainJobStatus::Queued | MainJobStatus::Running))
            .count(),
        1
    );
    release.notify_waiters();
    scheduler.join_inflight().await;
    Ok(())
}

#[tokio::test]
async fn given_conflicting_ready_issue_when_project_merge_approved_then_reconcile_blocks_release(
) -> anyhow::Result<()> {
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("project-merge-conflict-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let db = Db::open_memory().await?;
    let project_id = drive_project_with_repo(db.pool(), &repo.display().to_string()).await?;
    let issue_id = issues::create(db.pool(), project_id, "conflict", None, TS).await?;
    attach_conflicting_issue_branch(db.pool(), &repo, issue_id).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let err = scheduler
        .approve_project_merge(project_id, TS + 1)
        .await
        .expect_err("merge preflight must block predicted conflicts");

    assert!(
        err.to_string().contains("reconcile blocker"),
        "unexpected error: {err:#}"
    );
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::ReadyToMerge
    );
    Ok(())
}

#[tokio::test]
async fn given_done_reconcile_job_with_retry_proposal_when_applied_then_issue_retries(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    std::fs::create_dir_all(".tmp")?;
    let data_tmp = tempfile::Builder::new()
        .prefix("reconcile-apply-data-")
        .tempdir_in(".tmp")?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "retry", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Failed, TS).await?;
    agent_runs::start(
        db.pool(),
        StartRun {
            issue_id: Some(issue_id),
            main_job_id: None,
            role: Role::Plan,
            phase: "plan",
            agent_cmd: "plan {prompt}",
            status_before: Some(IssueStatus::Planning.as_str()),
            pid: None,
            prompt_path: None,
            log_path: None,
        },
        TS,
    )
    .await?;
    let proposal = format!(
        r#"```json
{{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": [
    {{
      "action": "retry_issue",
      "issue_id": {issue_id}
    }}
  ]
}}
```"#
    );
    let (main_job_id, _) = finished_reconcile_job_with_log(db.pool(), project_id, proposal).await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ScriptedAgent {
            db: db.clone(),
            now: TS + 4,
        }),
        Duration::from_secs(60),
    );

    let report = scheduler.apply_reconcile_job(main_job_id, TS + 3).await?;
    scheduler.join_inflight().await;

    assert_eq!(report.applied_count, 1);
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::PlanReady
    );
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_reconcile_apply_running_for_project_when_apply_again_then_project_guard_rejects(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    std::fs::create_dir_all(".tmp")?;
    let data_tmp = tempfile::Builder::new()
        .prefix("reconcile-apply-guard-data-")
        .tempdir_in(".tmp")?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());
    let repo_tmp = tempfile::Builder::new()
        .prefix("reconcile-apply-guard-repo-")
        .tempdir_in(".tmp")?;
    let repo = repo_tmp.path().join("repo");
    init_git_repo(&repo)?;
    let db = Db::open_memory().await?;
    let project_id = drive_project_with_repo(db.pool(), &repo.display().to_string()).await?;
    let issue_id = issues::create(db.pool(), project_id, "mark done guarded", None, TS).await?;
    attach_clean_issue_branch(db.pool(), &repo, issue_id, "guarded.txt").await?;
    issues::set_worktree(
        db.pool(),
        issue_id,
        Some(&branch_for_issue(issue_id)),
        Some("/tmp/auwsx-apply-guard"),
        TS,
    )
    .await?;
    git(
        &repo,
        &[
            "merge",
            "--no-ff",
            &branch_for_issue(issue_id),
            "-m",
            "guarded",
        ],
    )?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let (main_job_id, _) = finished_reconcile_job_with_log(
        db.pool(),
        project_id,
        format!(
            r#"```json
{{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": [{{ "action": "mark_done", "issue_id": {issue_id} }}]
}}
```"#
        ),
    )
    .await?;
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let scheduler = Arc::new(scheduler_with_worktrees(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Arc::new(BlockingTeardownWorktrees {
            started: started.clone(),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    ));
    let first = {
        let scheduler = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler.apply_reconcile_job(main_job_id, TS + 3).await })
    };
    started.notified().await;

    let err = scheduler
        .apply_reconcile_job(main_job_id, TS + 4)
        .await
        .expect_err("second apply must be serialized by project guard");

    assert!(
        err.to_string().contains("already reconciling or ticking"),
        "unexpected error: {err:#}"
    );
    release.notify_waiters();
    first.await??;
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_reconcile_retry_proposal_for_moved_issue_when_applied_then_rejected(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    std::fs::create_dir_all(".tmp")?;
    let data_tmp = tempfile::Builder::new()
        .prefix("reconcile-moved-data-")
        .tempdir_in(".tmp")?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let other_project_id = projects::create(
        db.pool(),
        NewProject {
            name: "other",
            repo_path: "/other",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "main {prompt}",
            route_agent_cmd: "main {prompt}",
            plan_agent_cmd: "plan {prompt}",
            work_agent_cmd: "work {prompt}",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Auto),
            plan_gate_timeout_min: Some(0),
            completion_soft_timeout_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let issue_id = issues::create(db.pool(), project_id, "moved", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Failed, TS).await?;
    let (main_job_id, _) = finished_reconcile_job_with_log(
        db.pool(),
        project_id,
        format!(
            r#"```json
{{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": [{{ "action": "retry_issue", "issue_id": {issue_id} }}]
}}
```"#
        ),
    )
    .await?;
    sqlx::query("UPDATE issues SET project_id = ? WHERE id = ?")
        .bind(other_project_id)
        .bind(issue_id)
        .execute(db.pool())
        .await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let err = scheduler
        .apply_reconcile_job(main_job_id, TS + 3)
        .await
        .expect_err("moved issue proposal must be stale");

    assert!(
        err.to_string().contains("stale_proposal"),
        "unexpected error: {err:#}"
    );
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::Failed
    );
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_unfenced_reconcile_job_log_when_applied_then_rejected_without_mutation(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    std::fs::create_dir_all(".tmp")?;
    let data_tmp = tempfile::Builder::new()
        .prefix("reconcile-unfenced-data-")
        .tempdir_in(".tmp")?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "unfenced", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Failed, TS).await?;
    let (main_job_id, _) = finished_reconcile_job_with_log(
        db.pool(),
        project_id,
        format!(
            r#"{{ "schema_version": 1, "kind": "auwsx_reconcile_proposal", "actions": [{{ "action": "retry_issue", "issue_id": {issue_id} }}] }}"#
        ),
    )
    .await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    scheduler
        .apply_reconcile_job(main_job_id, TS + 3)
        .await
        .expect_err("unfenced proposal must be rejected");

    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::Failed
    );
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_reconcile_job_with_unexpected_log_path_when_applied_then_rejected(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    std::fs::create_dir_all(".tmp")?;
    let data_tmp = tempfile::Builder::new()
        .prefix("reconcile-log-path-data-")
        .tempdir_in(".tmp")?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "log path", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Failed, TS).await?;
    let main_job_id =
        main_jobs::enqueue_project_reconcile(db.pool(), project_id, "prompt", TS).await?;
    let expected_log_path = artifacts::main_job_log_path(project_id, main_job_id, TS + 1)?;
    std::fs::write(&expected_log_path, "expected")?;
    let other_log_path = data_tmp.path().join("other.log");
    std::fs::write(
        &other_log_path,
        format!(
            r#"```json
{{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": [{{ "action": "retry_issue", "issue_id": {issue_id} }}]
}}
```"#
        ),
    )?;
    main_jobs::mark_running(
        db.pool(),
        main_job_id,
        TS + 1,
        &other_log_path.to_string_lossy(),
    )
    .await?;
    main_jobs::finish(db.pool(), main_job_id, MainJobStatus::Done, TS + 2, None).await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let err = scheduler
        .apply_reconcile_job(main_job_id, TS + 3)
        .await
        .expect_err("unexpected artifact path must be rejected");

    assert!(
        format!("{err:#}").contains("expected artifact path"),
        "unexpected error: {err:#}"
    );
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::Failed
    );
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_conflict_blocked_local_merge_when_auto_policy_ticks_then_ready_item_stays_queued(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let blocked = issues::create(db.pool(), project_id, "blocked", None, TS).await?;
    let ready = issues::create(db.pool(), project_id, "auto ready", None, TS).await?;
    issues::force_status(db.pool(), blocked, IssueStatus::ConflictBlocked, TS).await?;
    issues::force_status(db.pool(), ready, IssueStatus::ReadyToMerge, TS).await?;
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 1)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    scheduler.tick_project(project_id).await?;

    let ready_issue = issues::get(db.pool(), ready)
        .await?
        .expect("ready issue exists");
    assert_eq!(ready_issue.status, IssueStatus::ReadyToMerge);
    Ok(())
}

#[tokio::test]
async fn given_ready_project_when_executed_then_ready_merge_queue_is_approved() -> anyhow::Result<()>
{
    std::fs::create_dir_all(".tmp")?;
    let tmp = tempfile::Builder::new()
        .prefix("project-execute-")
        .tempdir_in(".tmp")?;
    let repo = tmp.path().join("repo");
    init_git_repo(&repo)?;
    let db = Db::open_memory().await?;
    let project_id = drive_project_with_repo(db.pool(), &repo.display().to_string()).await?;
    let issue_id = issues::create(db.pool(), project_id, "project execute ready", None, TS).await?;
    attach_clean_issue_branch(db.pool(), &repo, issue_id, "execute.txt").await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let release = Arc::new(Notify::new());
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 1)),
        Arc::new(BlockingAgent {
            started: Arc::new(Notify::new()),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );

    let outcome = scheduler.execute_project(project_id, TS + 1).await?;

    assert_eq!(
        outcome,
        ControlOutcome::ApprovedMerge {
            issue_ids: vec![issue_id]
        }
    );
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::Merging
    );
    release.notify_waiters();
    Ok(())
}

#[tokio::test]
async fn given_failed_issue_when_retried_then_last_actionable_phase_runs_again(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "retry me", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Working, TS).await?;
    let run_id = agent_runs::start(
        db.pool(),
        StartRun {
            issue_id: Some(issue_id),
            main_job_id: None,
            role: Role::Work,
            phase: IssueStatus::Working.as_str(),
            agent_cmd: "work {prompt}",
            status_before: Some(IssueStatus::Working.as_str()),
            pid: None,
            prompt_path: None,
            log_path: None,
        },
        TS,
    )
    .await?;
    agent_runs::finish(
        db.pool(),
        run_id,
        Some(IssueStatus::Failed.as_str()),
        Some(2),
        ExitKind::Exited,
        TS + 1,
        Some("failed before retry"),
    )
    .await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Failed, TS + 1).await?;
    let release = Arc::new(Notify::new());
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 2)),
        Arc::new(BlockingAgent {
            started: Arc::new(Notify::new()),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );

    scheduler.retry_failed_issue(issue_id, TS + 2).await?;

    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::Working
    );
    release.notify_waiters();
    Ok(())
}

#[tokio::test]
async fn given_actionable_issue_when_executed_then_phase_runs() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "run me", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Working, TS).await?;
    let release = Arc::new(Notify::new());
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 1)),
        Arc::new(BlockingAgent {
            started: Arc::new(Notify::new()),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );

    let outcome = scheduler.execute_issue(issue_id, TS + 1).await?;

    assert_eq!(outcome, ControlOutcome::RanIssue { issue_id });
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::Working
    );
    release.notify_waiters();
    Ok(())
}

#[tokio::test]
async fn given_failed_issue_when_executed_then_retry_runs() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "retry through execute", None, TS).await?;
    let run_id = agent_runs::start(
        db.pool(),
        StartRun {
            issue_id: Some(issue_id),
            main_job_id: None,
            role: Role::Review,
            phase: IssueStatus::Reviewing.as_str(),
            agent_cmd: "review {prompt}",
            status_before: Some(IssueStatus::Reviewing.as_str()),
            pid: None,
            prompt_path: None,
            log_path: None,
        },
        TS,
    )
    .await?;
    agent_runs::finish(
        db.pool(),
        run_id,
        Some(IssueStatus::Failed.as_str()),
        Some(1),
        ExitKind::Exited,
        TS + 1,
        Some("failed before execute retry"),
    )
    .await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Failed, TS + 1).await?;
    let release = Arc::new(Notify::new());
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 2)),
        Arc::new(BlockingAgent {
            started: Arc::new(Notify::new()),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );

    let outcome = scheduler.execute_issue(issue_id, TS + 2).await?;

    assert_eq!(outcome, ControlOutcome::RanIssue { issue_id });
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::Reviewing
    );
    release.notify_waiters();
    Ok(())
}

#[tokio::test]
async fn given_ready_issue_when_executed_then_merge_is_approved() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "merge me", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS).await?;
    let release = Arc::new(Notify::new());
    let scheduler = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 1)),
        Arc::new(BlockingAgent {
            started: Arc::new(Notify::new()),
            release: release.clone(),
        }),
        Duration::from_secs(60),
    );

    let outcome = scheduler.execute_issue(issue_id, TS + 1).await?;

    assert_eq!(
        outcome,
        ControlOutcome::ApprovedMerge {
            issue_ids: vec![issue_id]
        }
    );
    assert_eq!(
        issues::get(db.pool(), issue_id)
            .await?
            .expect("issue exists")
            .status,
        IssueStatus::Merging
    );
    release.notify_waiters();
    Ok(())
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
    scheduler_runs::recent_by_project(pool, project_id, 100).await
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
    agent_runs::recent_by_project(pool, project_id, 100).await
}

async fn wait_for_main_jobs(
    pool: &SqlitePool,
    project_id: i64,
    min: usize,
) -> anyhow::Result<Vec<auwsx_core::main_jobs::MainJob>> {
    for _ in 0..100 {
        let jobs = main_jobs::recent_by_project(pool, project_id, 100).await?;
        if jobs.len() >= min {
            return Ok(jobs);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    main_jobs::recent_by_project(pool, project_id, 100).await
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
    let mut status = IssueStatus::New;
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
    assert_eq!(
        runs[0].status_before.as_deref(),
        Some(IssueStatus::Planning.as_str()),
        "pipeline must enter PLANNING before spawning the planner"
    );
    assert!(
        runs.iter()
            .all(|r| r.status_before.as_deref() != Some(IssueStatus::New.as_str())),
        "NEW is a scheduler eligibility marker, not an agent run phase"
    );
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
        assert!(
            r.phase_report
                .as_deref()
                .is_some_and(|report| report.contains("completed by scripted agent")),
            "run {} missing phase report",
            r.id
        );
    }
    let jobs = main_jobs::recent_by_project(db.pool(), project_id, 10).await?;
    assert!(
        jobs.iter()
            .any(|job| job.source == main_jobs::MainJobSource::PostMerge
                && job.kind == "dream"
                && job.status == MainJobStatus::Done),
        "DONE issue must enqueue and run post-merge dream"
    );

    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_deepsleep_due_when_auto_ticks_then_memory_job_runs_once() -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_deepsleep_interval(db.pool(), project_id, 7, None).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, Some("@tick")).await?;
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

    let _ = wait_for_main_jobs(db.pool(), project_id, 1).await?;
    handle.abort();
    let _ = handle.await;
    sched.join_inflight().await;

    let jobs = main_jobs::recent_by_project(db.pool(), project_id, 10).await?;
    let deepsleep_jobs = jobs
        .iter()
        .filter(|job| job.kind == "deepsleep" && job.status == MainJobStatus::Done)
        .count();
    let project = projects::get(db.pool(), project_id)
        .await?
        .expect("project exists");
    assert_eq!(deepsleep_jobs, 1);
    assert_eq!(project.last_deepsleep_at, Some(TS));

    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_deepsleep_not_due_when_auto_ticks_then_no_memory_job_runs() -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_deepsleep_interval(db.pool(), project_id, 7, Some(TS)).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, Some("@tick")).await?;
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
    sched.join_inflight().await;

    let jobs = main_jobs::recent_by_project(db.pool(), project_id, 10).await?;
    assert!(
        jobs.iter().all(|job| job.kind != "deepsleep"),
        "not-due deepsleep must not enqueue a memory job"
    );

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
async fn given_remote_enabled_project_when_backlog_routes_then_remote_issue_sync_is_queued(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    set_project_runtime_policy(db.pool(), project_id, 0, None).await?;
    enable_project_remote(db.pool(), project_id).await?;
    backlog::add(
        db.pool(),
        project_id,
        "create a remote mirrored issue",
        Source::Human,
        None,
        TS,
    )
    .await?;
    let remote_executor = Arc::new(TestRemoteExecutor::default());
    let sched = scheduler_with_remote_executor(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        remote_executor,
        Duration::from_secs(60),
    );

    sched.tick_project(project_id).await?;

    let runs = remote::recent_sync_runs(db.pool(), project_id, 10).await?;
    let issue_run = runs
        .iter()
        .find(|run| run.kind == RemoteSyncKind::Issue)
        .expect("issue sync run exists");
    let issues = issues::list_by_project(db.pool(), project_id).await?;
    let issue_link = remote::issue_link_by_issue(db.pool(), issues[0].id)
        .await?
        .expect("remote issue link exists");
    assert_eq!(
        (issue_run.status, issue_link.remote_issue_number),
        (RemoteSyncStatus::Done, 177)
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
    assert_eq!(runs[0].role, Role::Plan);
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
            output_route: RoutineType::Report,
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
            arsenal_preset_name: None,
            main_agent_cmd: "main {prompt}",
            route_agent_cmd: "main {prompt}",
            plan_agent_cmd: "plan {prompt}",
            work_agent_cmd: "work {prompt}",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Auto),
            plan_gate_timeout_min: Some(0),
            completion_soft_timeout_min: None,
            schedule_cron: None,
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
    set_project_runtime_policy(db.pool(), project_id, 0, Some("@tick")).await?;
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
    set_project_runtime_policy(db.pool(), project_id, 0, Some("0 * * * *")).await?;
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
    set_project_runtime_policy(db.pool(), project_id, 0, Some("* * * * *")).await?;
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
    set_project_runtime_policy(db.pool(), project_id, 0, Some("0 * * * *")).await?;
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
async fn given_nonzero_agent_without_status_change_when_tick_project_then_issue_failed(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "nonzero", None, TS).await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(NonzeroNoopAgent),
        Duration::from_secs(60),
    );

    sched.tick_project(project_id).await?;
    sched.join_inflight().await;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    let runs = agent_runs::list_by_issue(db.pool(), issue_id).await?;
    assert_eq!(issue.status, IssueStatus::Failed);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].exit_code, Some(2));
    assert_eq!(
        runs[0].status_after.as_deref(),
        Some(IssueStatus::Failed.as_str())
    );
    assert!(runs[0]
        .note
        .as_deref()
        .unwrap_or("")
        .contains("without changing issue status"));
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

#[tokio::test]
async fn given_zero_agent_without_status_change_when_tick_project_then_issue_failed(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "no status", None, TS).await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    sched.tick_project(project_id).await?;
    sched.join_inflight().await;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    let runs = agent_runs::list_by_issue(db.pool(), issue_id).await?;
    assert_eq!(issue.status, IssueStatus::Failed);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].exit_kind, Some(ExitKind::Exited));
    assert_eq!(runs[0].exit_code, Some(0));
    assert_eq!(
        runs[0].status_after.as_deref(),
        Some(IssueStatus::Failed.as_str())
    );
    assert!(runs[0]
        .note
        .as_deref()
        .unwrap_or("")
        .contains("exited without changing issue status"));
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

#[tokio::test]
async fn given_done_issue_when_abandoned_then_noop() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "done", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Done, TS).await?;
    issues::set_worktree(db.pool(), issue_id, Some("br"), Some("/wt"), TS).await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    sched.abandon_issue(issue_id, TS).await?;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    assert_eq!(
        (issue.status, issue.worktree_path.as_deref()),
        (IssueStatus::Done, Some("/wt"))
    );
    Ok(())
}

#[tokio::test]
async fn given_failed_issue_when_abandoned_then_noop() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "failed", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Failed, TS).await?;
    issues::set_worktree(db.pool(), issue_id, Some("br"), Some("/wt"), TS).await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    sched.abandon_issue(issue_id, TS).await?;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    assert_eq!(
        (issue.status, issue.worktree_path.as_deref()),
        (IssueStatus::Failed, Some("/wt"))
    );
    Ok(())
}

#[tokio::test]
async fn given_working_issue_with_worktree_when_abandoned_then_status_abandoned(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "working", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Working, TS).await?;
    issues::set_worktree(db.pool(), issue_id, Some("br"), Some("/wt"), TS).await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    sched.abandon_issue(issue_id, TS + 1).await?;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    assert_eq!(
        (issue.status, issue.branch, issue.worktree_path),
        (IssueStatus::Abandoned, None, None)
    );
    Ok(())
}

#[tokio::test]
async fn given_failed_issue_with_worktree_when_cleanup_then_worktree_fields_cleared(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "failed", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::Failed, TS).await?;
    issues::set_worktree(db.pool(), issue_id, Some("br"), Some("/wt"), TS).await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    sched.cleanup_issue_worktree_by_id(issue_id).await?;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    assert_eq!(
        (issue.status, issue.branch, issue.worktree_path),
        (IssueStatus::Failed, None, None)
    );
    Ok(())
}

#[tokio::test]
async fn given_open_issue_run_when_recovered_then_run_closed_and_issue_failed() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "interrupted", None, TS).await?;
    issues::transition(db.pool(), issue_id, IssueStatus::Planning, TS).await?;
    let run_id = agent_runs::start(
        db.pool(),
        StartRun {
            issue_id: Some(issue_id),
            main_job_id: None,
            role: Role::Plan,
            phase: IssueStatus::Planning.as_str(),
            agent_cmd: "plan {prompt}",
            status_before: Some(IssueStatus::Planning.as_str()),
            pid: Some(12345),
            prompt_path: None,
            log_path: None,
        },
        TS,
    )
    .await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 1)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let recovered = sched.recover_open_issue_runs(TS + 1).await?;

    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    let run = agent_runs::get(db.pool(), run_id)
        .await?
        .expect("run exists");
    assert_eq!(
        (recovered, issue.status, run.exit_kind, run.exited_at),
        (1, IssueStatus::Failed, Some(ExitKind::Error), Some(TS + 1))
    );
    assert_eq!(
        run.status_after.as_deref(),
        Some(IssueStatus::Failed.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn given_open_issue_run_already_advanced_to_human_gate_when_recovered_then_status_preserved(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "advanced", None, TS).await?;
    issues::force_status(db.pool(), issue_id, IssueStatus::PlanReady, TS + 1).await?;
    let run_id = agent_runs::start(
        db.pool(),
        StartRun {
            issue_id: Some(issue_id),
            main_job_id: None,
            role: Role::Plan,
            phase: IssueStatus::Planning.as_str(),
            agent_cmd: "plan",
            status_before: Some(IssueStatus::Planning.as_str()),
            pid: Some(123),
            prompt_path: None,
            log_path: None,
        },
        TS,
    )
    .await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 2)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let recovered = sched.recover_open_issue_runs(TS + 2).await?;
    let issue = issues::get(db.pool(), issue_id)
        .await?
        .expect("issue exists");
    let run = agent_runs::get(db.pool(), run_id)
        .await?
        .expect("run exists");

    assert_eq!(recovered, 1);
    assert_eq!(issue.status, IssueStatus::PlanReady);
    assert_eq!(
        run.status_after.as_deref(),
        Some(IssueStatus::PlanReady.as_str())
    );
    assert_eq!(run.exit_kind, Some(ExitKind::Error));
    Ok(())
}

#[tokio::test]
async fn given_running_main_job_when_recovered_then_run_closed_and_job_failed() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let routine_id = routines::create(
        db.pool(),
        NewRoutine {
            project_id,
            name: "daily report",
            output_route: RoutineType::Report,
            prompt: "write a report",
            cron: "0 0 * * * *",
            writable_paths: None,
            enabled: true,
        },
        TS,
    )
    .await?;
    let main_job_id =
        main_jobs::enqueue_routine(db.pool(), project_id, routine_id, "report", "prompt", TS)
            .await?;
    main_jobs::mark_running(db.pool(), main_job_id, TS, "/tmp/main.log").await?;
    let run_id = agent_runs::start(
        db.pool(),
        StartRun {
            issue_id: None,
            main_job_id: Some(main_job_id),
            role: Role::Main,
            phase: "routine",
            agent_cmd: "main {prompt}",
            status_before: Some(MainJobStatus::Queued.as_str()),
            pid: Some(12345),
            prompt_path: None,
            log_path: Some("/tmp/main.log"),
        },
        TS,
    )
    .await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 1)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let recovered = sched.recover_open_main_jobs(TS + 1).await?;

    let job = main_jobs::get(db.pool(), main_job_id)
        .await?
        .expect("job exists");
    let run = agent_runs::get(db.pool(), run_id)
        .await?
        .expect("run exists");
    assert_eq!(
        (recovered, job.status, run.exit_kind, run.exited_at),
        (
            1,
            MainJobStatus::Failed,
            Some(ExitKind::Error),
            Some(TS + 1)
        )
    );
    assert_eq!(
        run.status_after.as_deref(),
        Some(MainJobStatus::Failed.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn given_terminal_main_job_with_open_run_when_recovered_then_job_status_is_preserved(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let routine_id = routines::create(
        db.pool(),
        NewRoutine {
            project_id,
            name: "daily report",
            output_route: RoutineType::Report,
            prompt: "write a report",
            cron: "0 0 * * * *",
            writable_paths: None,
            enabled: true,
        },
        TS,
    )
    .await?;
    let main_job_id =
        main_jobs::enqueue_routine(db.pool(), project_id, routine_id, "report", "prompt", TS)
            .await?;
    main_jobs::mark_running(db.pool(), main_job_id, TS, "/tmp/main.log").await?;
    main_jobs::finish(
        db.pool(),
        main_job_id,
        MainJobStatus::Done,
        TS + 1,
        Some("ok"),
    )
    .await?;
    let run_id = agent_runs::start(
        db.pool(),
        StartRun {
            issue_id: None,
            main_job_id: Some(main_job_id),
            role: Role::Main,
            phase: "routine",
            agent_cmd: "main {prompt}",
            status_before: Some(MainJobStatus::Queued.as_str()),
            pid: Some(12345),
            prompt_path: None,
            log_path: Some("/tmp/main.log"),
        },
        TS,
    )
    .await?;
    let sched = scheduler_with(
        db.clone(),
        Arc::new(FixedClock(TS + 2)),
        Arc::new(ExitAgent),
        Duration::from_secs(60),
    );

    let recovered = sched.recover_open_main_jobs(TS + 2).await?;

    let job = main_jobs::get(db.pool(), main_job_id)
        .await?
        .expect("job exists");
    let run = agent_runs::get(db.pool(), run_id)
        .await?
        .expect("run exists");
    assert_eq!(recovered, 1);
    assert_eq!(job.status, MainJobStatus::Done);
    assert_eq!(
        run.status_after.as_deref(),
        Some(MainJobStatus::Done.as_str())
    );
    assert_eq!(run.exit_kind, Some(ExitKind::Error));
    Ok(())
}

#[tokio::test]
async fn given_running_issue_when_remove_project_then_non_shallow_err_but_shallow_removes_project(
) -> anyhow::Result<()> {
    let _env_guard = ENV_LOCK.lock().await;
    let data_tmp = tempfile::tempdir()?;
    std::env::set_var("AUWSX_DATA_DIR", data_tmp.path());

    let db = Db::open_memory().await?;
    let project_id = drive_project(db.pool()).await?;
    let issue_id = issues::create(db.pool(), project_id, "running", None, TS).await?;
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
        .remove_project(project_id, false)
        .await
        .expect_err("non-shallow removal must reject a running issue");
    assert!(err.to_string().contains("running issue"));

    sched.remove_project(project_id, true).await?;
    let project = projects::get(db.pool(), project_id).await?;
    assert_eq!(project, None);

    release.notify_waiters();
    sched.join_inflight().await;
    std::env::remove_var("AUWSX_DATA_DIR");
    Ok(())
}

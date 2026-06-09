//! Integration tests for the auwsx-core async DB CRUD API.
//!
//! Source of truth: crates/auwsx-core/src/{db/projects,db/issues,db/findings,
//! db/subtasks,backlog,steering}.rs + state.rs.
//!
//! These assert the PUBLIC CONTRACT only, via the public CRUD fns + get/list.
//! Failure cases are tested aggressively: missing ids, empty results, boundary
//! bumps, the documented guards (steering working-phase guard both directions;
//! transition legal vs illegal), and enum round-trips. Timestamps are proven by
//! asserting the resolved_at/done_at value equals the `now` that was passed.

use auwsx_core::backlog::{self, Approval, Source};
use auwsx_core::db::findings::{self, FindingStatus, NewFinding, Severity};
use auwsx_core::db::issues::{self, INITIAL_STATUS};
use auwsx_core::db::projects::{self, CompletionPolicy, MergeMode, NewProject};
use auwsx_core::db::subtasks;
use auwsx_core::db::Db;
use auwsx_core::state::{is_legal_transition, IssueStatus};
use auwsx_core::steering::{self, SteeringSource};
use sqlx::SqlitePool;

/// Fixed deterministic timestamp (Unix epoch ms). No SystemTime.
const TS: i64 = 1_000_000;
/// A DISTINCT, strictly-later timestamp, used to prove a second write stamped a
/// different clock value than the first.
const TS2: i64 = 2_000_000;

// ---------------------------------------------------------------------------
// Helpers. A project must exist before an issue (FK); an issue before
// findings/subtasks/steering. These supply every required field with a valid
// value so a test can never accidentally omit one.
// ---------------------------------------------------------------------------

/// Insert a valid project via the public `projects::create`, returning its id.
async fn insert_project(pool: &SqlitePool, name: &str) -> anyhow::Result<i64> {
    projects::create(
        pool,
        NewProject {
            name,
            repo_path: "/repo/path",
            default_branch: "main",
            main_agent_cmd: "claude {prompt}",
            plan_agent_cmd: "claude-plan {prompt}",
            work_agent_cmd: "claude-work {prompt}",
            review_agent_cmd: None,
        },
        TS,
    )
    .await
}

/// Insert a valid `routines` row directly (no public CRUD module here), so
/// backlog items can reference a real `origin_routine_id` FK target.
async fn insert_routine(pool: &SqlitePool, project_id: i64) -> anyhow::Result<i64> {
    use sqlx::Row;
    let id: i64 = sqlx::query(
        "INSERT INTO routines (project_id, name, origin, type, prompt, cron, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(project_id)
    .bind("r")
    .bind("user")
    .bind("report")
    .bind("a prompt")
    .bind("0 0 * * * *")
    .bind(TS)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

/// Create an issue and force it into `status` (bypassing the legality matrix),
/// returning its id. Use for setting up a test fixture in an arbitrary phase.
async fn insert_issue_at(
    pool: &SqlitePool,
    project_id: i64,
    status: IssueStatus,
) -> anyhow::Result<i64> {
    let id = issues::create(pool, project_id, "a title", None, TS).await?;
    issues::force_status(pool, id, status, TS).await?;
    Ok(id)
}

// ===========================================================================
// projects
// ===========================================================================

#[tokio::test]
async fn given_minimal_new_project_when_created_then_get_returns_supplied_fields(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "alpha").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.name, "alpha");
    Ok(())
}

#[tokio::test]
async fn given_new_project_when_created_then_created_at_is_supplied_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "alpha").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.created_at, TS);
    Ok(())
}

#[tokio::test]
async fn given_review_agent_cmd_none_when_created_then_review_agent_cmd_is_none(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "alpha").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.review_agent_cmd, None);
    Ok(())
}

#[tokio::test]
async fn given_review_agent_cmd_some_when_created_then_review_agent_cmd_roundtrips(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "alpha",
            repo_path: "/r",
            default_branch: "main",
            main_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: Some("reviewer {prompt}"),
        },
        TS,
    )
    .await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.review_agent_cmd.as_deref(), Some("reviewer {prompt}"));
    Ok(())
}

// --- SQL DEFAULTs after a minimal create -----------------------------------

#[tokio::test]
async fn given_minimal_create_when_get_then_completion_policy_defaults_manual(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.completion_policy, CompletionPolicy::Manual);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_completion_soft_timeout_defaults_60(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.completion_soft_timeout_min, 60);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_plan_gate_timeout_defaults_10() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.plan_gate_timeout_min, 10);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_iteration_timeout_defaults_30() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.iteration_timeout_min, 30);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_main_job_timeout_defaults_60() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.main_job_timeout_min, 60);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_review_max_rounds_defaults_5() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.review_max_rounds, 5);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_conflict_max_attempts_defaults_3() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.conflict_max_attempts, 3);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_max_concurrency_defaults_1() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.max_concurrency, 1);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_schedule_interval_defaults_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.schedule_interval_min, None);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_merge_mode_defaults_local() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.merge_mode, MergeMode::Local);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_skill_path_defaults_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.skill_path, None);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_deepsleep_interval_defaults_7() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.deepsleep_interval_days, 7);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_last_deepsleep_at_defaults_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.last_deepsleep_at, None);
    Ok(())
}

#[tokio::test]
async fn given_no_project_with_id_when_get_then_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    assert_eq!(projects::get(db.pool(), 999_999).await?, None);
    Ok(())
}

#[tokio::test]
async fn given_project_when_get_by_name_then_returns_same_row() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "alpha").await?;
    let by_id = projects::get(db.pool(), id).await?.expect("by id");
    let by_name = projects::get_by_name(db.pool(), "alpha").await?.expect("by name");
    assert_eq!(by_id, by_name);
    Ok(())
}

#[tokio::test]
async fn given_no_project_with_name_when_get_by_name_then_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    assert_eq!(projects::get_by_name(db.pool(), "nope").await?, None);
    Ok(())
}

#[tokio::test]
async fn given_no_projects_when_list_then_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    assert!(projects::list(db.pool()).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_three_projects_when_list_then_ordered_by_id_asc() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    insert_project(db.pool(), "a").await?;
    insert_project(db.pool(), "b").await?;
    insert_project(db.pool(), "c").await?;
    let names: Vec<String> = projects::list(db.pool()).await?.into_iter().map(|p| p.name).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
    Ok(())
}

#[tokio::test]
async fn given_existing_name_when_create_again_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    insert_project(db.pool(), "dup").await?;
    let res = insert_project(db.pool(), "dup").await;
    assert!(res.is_err(), "name is UNIQUE; second create must Err");
    Ok(())
}

// --- MergeMode / CompletionPolicy enum round-trips -------------------------

#[tokio::test]
async fn given_merge_mode_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for v in [MergeMode::Local, MergeMode::Pr] {
        assert_eq!(MergeMode::from_str(v.as_str()), Some(v), "{v:?}");
    }
    Ok(())
}

#[tokio::test]
async fn given_merge_mode_as_str_when_checked_then_matches_spec_ids() -> anyhow::Result<()> {
    assert_eq!(MergeMode::Local.as_str(), "local");
    assert_eq!(MergeMode::Pr.as_str(), "pr");
    Ok(())
}

#[tokio::test]
async fn given_bogus_or_empty_when_merge_mode_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(MergeMode::from_str("bogus"), None);
    assert_eq!(MergeMode::from_str(""), None);
    Ok(())
}

#[tokio::test]
async fn given_completion_policy_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for v in [CompletionPolicy::Manual, CompletionPolicy::Soft, CompletionPolicy::Auto] {
        assert_eq!(CompletionPolicy::from_str(v.as_str()), Some(v), "{v:?}");
    }
    Ok(())
}

#[tokio::test]
async fn given_completion_policy_as_str_when_checked_then_matches_spec_ids() -> anyhow::Result<()> {
    assert_eq!(CompletionPolicy::Manual.as_str(), "manual");
    assert_eq!(CompletionPolicy::Soft.as_str(), "soft");
    assert_eq!(CompletionPolicy::Auto.as_str(), "auto");
    Ok(())
}

#[tokio::test]
async fn given_bogus_or_empty_when_completion_policy_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(CompletionPolicy::from_str("bogus"), None);
    assert_eq!(CompletionPolicy::from_str(""), None);
    Ok(())
}

// ===========================================================================
// issues
// ===========================================================================

#[tokio::test]
async fn given_new_issue_when_created_then_status_is_initial_consolidating(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.status, INITIAL_STATUS);
    Ok(())
}

#[tokio::test]
async fn given_initial_status_const_when_checked_then_equals_consolidating() -> anyhow::Result<()> {
    assert_eq!(INITIAL_STATUS, IssueStatus::Consolidating);
    Ok(())
}

#[tokio::test]
async fn given_new_issue_with_description_when_get_then_description_roundtrips(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", Some("the why"), TS).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.description.as_deref(), Some("the why"));
    Ok(())
}

#[tokio::test]
async fn given_new_issue_without_description_when_get_then_description_none(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.description, None);
    Ok(())
}

#[tokio::test]
async fn given_new_issue_when_created_then_counters_and_flags_default(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    // One logical fact: a fresh issue carries no review rounds yet.
    assert_eq!(issue.review_round, 0);
    Ok(())
}

#[tokio::test]
async fn given_new_issue_when_created_then_has_pending_steering_false() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert!(!issue.has_pending_steering);
    Ok(())
}

#[tokio::test]
async fn given_no_issue_with_id_when_get_then_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    assert!(issues::get(db.pool(), 999_999).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn given_no_issues_when_list_by_project_then_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    assert!(issues::list_by_project(db.pool(), pid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_three_issues_when_list_by_project_then_newest_id_first() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let a = issues::create(db.pool(), pid, "a", None, TS).await?;
    let b = issues::create(db.pool(), pid, "b", None, TS).await?;
    let c = issues::create(db.pool(), pid, "c", None, TS).await?;
    let ids: Vec<i64> =
        issues::list_by_project(db.pool(), pid).await?.into_iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![c, b, a]);
    Ok(())
}

#[tokio::test]
async fn given_issues_in_other_project_when_list_by_project_then_excluded(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let p1 = insert_project(db.pool(), "p1").await?;
    let p2 = insert_project(db.pool(), "p2").await?;
    issues::create(db.pool(), p2, "other", None, TS).await?;
    let mine = issues::create(db.pool(), p1, "mine", None, TS).await?;
    let ids: Vec<i64> =
        issues::list_by_project(db.pool(), p1).await?.into_iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![mine]);
    Ok(())
}

#[tokio::test]
async fn given_issues_of_mixed_status_when_list_by_status_then_only_that_status(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let planning = insert_issue_at(db.pool(), pid, IssueStatus::Planning).await?;
    insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let ids: Vec<i64> = issues::list_by_status(db.pool(), pid, IssueStatus::Planning)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, vec![planning]);
    Ok(())
}

#[tokio::test]
async fn given_no_issues_in_status_when_list_by_status_then_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let got = issues::list_by_status(db.pool(), pid, IssueStatus::Done).await?;
    assert!(got.is_empty());
    Ok(())
}

// --- transition: legal, illegal, timestamp ---------------------------------

#[tokio::test]
async fn given_consolidating_when_transition_to_planning_then_status_planning(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::transition(db.pool(), id, IssueStatus::Planning, TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.status, IssueStatus::Planning);
    Ok(())
}

#[tokio::test]
async fn given_legal_transition_when_applied_then_updated_at_is_new_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::transition(db.pool(), id, IssueStatus::Planning, TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.updated_at, TS2);
    Ok(())
}

#[tokio::test]
async fn given_consolidating_when_transition_to_done_then_err() -> anyhow::Result<()> {
    // Structurally valid but semantically wrong: CONSOLIDATING -> DONE is illegal.
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let res = issues::transition(db.pool(), id, IssueStatus::Done, TS2).await;
    assert!(res.is_err(), "CONSOLIDATING -> DONE must be rejected");
    Ok(())
}

#[tokio::test]
async fn given_illegal_transition_when_attempted_then_status_unchanged() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let _ = issues::transition(db.pool(), id, IssueStatus::Implementing, TS2).await;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.status, IssueStatus::Consolidating);
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_transition_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = issues::transition(db.pool(), 999_999, IssueStatus::Planning, TS).await;
    assert!(res.is_err(), "transition on missing id must Err");
    Ok(())
}

// --- force_status: bypasses legality ---------------------------------------

#[tokio::test]
async fn given_illegal_edge_when_force_status_then_applied_anyway() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    // CONSOLIDATING -> DONE is illegal for transition, but force bypasses the matrix.
    issues::force_status(db.pool(), id, IssueStatus::Done, TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.status, IssueStatus::Done);
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_force_status_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = issues::force_status(db.pool(), 999_999, IssueStatus::Done, TS).await;
    assert!(res.is_err(), "force_status on missing id must Err");
    Ok(())
}

// --- mark_absorbed ---------------------------------------------------------

#[tokio::test]
async fn given_consolidating_issue_when_mark_absorbed_then_status_absorbed(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let donor = issues::create(db.pool(), pid, "donor", None, TS).await?;
    let target = issues::create(db.pool(), pid, "target", None, TS).await?;
    issues::mark_absorbed(db.pool(), donor, target, TS2).await?;
    let issue = issues::get(db.pool(), donor).await?.expect("issue exists");
    assert_eq!(issue.status, IssueStatus::Absorbed);
    Ok(())
}

#[tokio::test]
async fn given_consolidating_issue_when_mark_absorbed_then_absorbed_into_id_set(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let donor = issues::create(db.pool(), pid, "donor", None, TS).await?;
    let target = issues::create(db.pool(), pid, "target", None, TS).await?;
    issues::mark_absorbed(db.pool(), donor, target, TS2).await?;
    let issue = issues::get(db.pool(), donor).await?.expect("issue exists");
    assert_eq!(issue.absorbed_into_id, Some(target));
    Ok(())
}

#[tokio::test]
async fn given_non_consolidating_issue_when_mark_absorbed_then_err() -> anyhow::Result<()> {
    // ->ABSORBED is legal only from CONSOLIDATING; from PLANNING it must Err.
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let donor = insert_issue_at(db.pool(), pid, IssueStatus::Planning).await?;
    let res = issues::mark_absorbed(db.pool(), donor, 1, TS2).await;
    assert!(res.is_err(), "mark_absorbed from PLANNING must Err");
    Ok(())
}

// --- set_worktree ----------------------------------------------------------

#[tokio::test]
async fn given_issue_when_set_worktree_then_fields_roundtrip() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::set_worktree(db.pool(), id, Some("br"), Some("/wt"), Some("sess"), TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(
        (issue.branch.as_deref(), issue.worktree_path.as_deref(), issue.agent_session.as_deref()),
        (Some("br"), Some("/wt"), Some("sess"))
    );
    Ok(())
}

#[tokio::test]
async fn given_issue_when_set_worktree_all_none_then_fields_null() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::set_worktree(db.pool(), id, None, None, None, TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!((issue.branch, issue.worktree_path, issue.agent_session), (None, None, None));
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_set_worktree_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = issues::set_worktree(db.pool(), 999_999, None, None, None, TS).await;
    assert!(res.is_err(), "set_worktree on missing id must Err");
    Ok(())
}

// --- set_pending_steering --------------------------------------------------

#[tokio::test]
async fn given_issue_when_set_pending_steering_true_then_flag_set() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::set_pending_steering(db.pool(), id, true, TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert!(issue.has_pending_steering);
    Ok(())
}

#[tokio::test]
async fn given_pending_steering_set_when_cleared_then_flag_false() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::set_pending_steering(db.pool(), id, true, TS2).await?;
    issues::set_pending_steering(db.pool(), id, false, TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert!(!issue.has_pending_steering);
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_set_pending_steering_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = issues::set_pending_steering(db.pool(), 999_999, true, TS).await;
    assert!(res.is_err(), "set_pending_steering on missing id must Err");
    Ok(())
}

// --- set_wait_until --------------------------------------------------------

#[tokio::test]
async fn given_issue_when_set_wait_until_some_then_roundtrips() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::set_wait_until(db.pool(), id, Some(TS2), TS).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.wait_until, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_wait_until_set_when_cleared_with_none_then_null() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::set_wait_until(db.pool(), id, Some(TS2), TS).await?;
    issues::set_wait_until(db.pool(), id, None, TS).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.wait_until, None);
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_set_wait_until_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = issues::set_wait_until(db.pool(), 999_999, Some(TS), TS).await;
    assert!(res.is_err(), "set_wait_until on missing id must Err");
    Ok(())
}

// --- bump_review_round -----------------------------------------------------

#[tokio::test]
async fn given_fresh_issue_when_bump_review_round_then_returns_1() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let new = issues::bump_review_round(db.pool(), id, TS2).await?;
    assert_eq!(new, 1, "review_round starts at 0; first bump returns 1");
    Ok(())
}

#[tokio::test]
async fn given_twice_bumped_review_round_when_bumped_then_returns_2() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::bump_review_round(db.pool(), id, TS2).await?;
    let new = issues::bump_review_round(db.pool(), id, TS2).await?;
    assert_eq!(new, 2);
    Ok(())
}

#[tokio::test]
async fn given_bumped_review_round_when_get_then_persisted_value_matches(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::bump_review_round(db.pool(), id, TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.review_round, 1);
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_bump_review_round_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = issues::bump_review_round(db.pool(), 999_999, TS).await;
    assert!(res.is_err(), "bump_review_round on missing id must Err");
    Ok(())
}

// --- bump_conflict_attempts ------------------------------------------------

#[tokio::test]
async fn given_fresh_issue_when_bump_conflict_attempts_then_returns_1() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let new = issues::bump_conflict_attempts(db.pool(), id, TS2).await?;
    assert_eq!(new, 1, "conflict_attempts starts at 0; first bump returns 1");
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_bump_conflict_attempts_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = issues::bump_conflict_attempts(db.pool(), 999_999, TS).await;
    assert!(res.is_err(), "bump_conflict_attempts on missing id must Err");
    Ok(())
}

// --- IssueStatus::accepts_steering set -------------------------------------

#[tokio::test]
async fn given_working_phase_statuses_when_accepts_steering_then_true() -> anyhow::Result<()> {
    for v in [
        IssueStatus::Implementing,
        IssueStatus::Review,
        IssueStatus::NeedsFix,
        IssueStatus::Audit,
    ] {
        assert!(v.accepts_steering(), "{v:?} should accept steering");
    }
    Ok(())
}

#[tokio::test]
async fn given_non_working_phase_status_when_accepts_steering_then_false() -> anyhow::Result<()> {
    // Planned is structurally a real status but not a working phase.
    assert!(!IssueStatus::Planned.accepts_steering());
    Ok(())
}

// --- is_legal_transition sample contract -----------------------------------

#[tokio::test]
async fn given_consolidating_to_planning_when_checked_then_legal() -> anyhow::Result<()> {
    assert!(is_legal_transition(IssueStatus::Consolidating, IssueStatus::Planning));
    Ok(())
}

#[tokio::test]
async fn given_consolidating_to_implementing_when_checked_then_illegal() -> anyhow::Result<()> {
    assert!(!is_legal_transition(IssueStatus::Consolidating, IssueStatus::Implementing));
    Ok(())
}

#[tokio::test]
async fn given_consolidating_to_absorbed_when_checked_then_legal() -> anyhow::Result<()> {
    assert!(is_legal_transition(IssueStatus::Consolidating, IssueStatus::Absorbed));
    Ok(())
}

#[tokio::test]
async fn given_consolidating_to_done_when_checked_then_illegal() -> anyhow::Result<()> {
    assert!(!is_legal_transition(IssueStatus::Consolidating, IssueStatus::Done));
    Ok(())
}

// --- IssueStatus enum round-trip -------------------------------------------

#[tokio::test]
async fn given_bogus_or_empty_when_issue_status_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(IssueStatus::from_str("bogus"), None);
    assert_eq!(IssueStatus::from_str(""), None);
    Ok(())
}

// ===========================================================================
// findings
// ===========================================================================

/// Build a minimal `NewFinding` for an issue at review round 0.
fn new_finding(issue_id: i64) -> NewFinding<'static> {
    NewFinding {
        issue_id,
        review_round: 0,
        severity: Severity::Major,
        lens: None,
        title: "a finding",
        detail: None,
        file_ref: None,
    }
}

#[tokio::test]
async fn given_new_finding_when_added_then_status_open() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.status, FindingStatus::Open);
    Ok(())
}

#[tokio::test]
async fn given_new_finding_when_added_then_resolved_at_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.resolved_at, None);
    Ok(())
}

#[tokio::test]
async fn given_finding_with_optional_fields_when_get_then_roundtrip() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(
        db.pool(),
        NewFinding {
            issue_id: iid,
            review_round: 2,
            severity: Severity::Blocker,
            lens: Some("security"),
            title: "leak",
            detail: Some("token logged"),
            file_ref: Some("src/x.rs:10"),
        },
        TS,
    )
    .await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(
        (f.review_round, f.severity, f.lens.as_deref(), f.detail.as_deref(), f.file_ref.as_deref()),
        (2, Severity::Blocker, Some("security"), Some("token logged"), Some("src/x.rs:10"))
    );
    Ok(())
}

#[tokio::test]
async fn given_no_finding_with_id_when_get_then_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    assert!(findings::get(db.pool(), 999_999).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn given_no_findings_when_list_by_issue_then_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    assert!(findings::list_by_issue(db.pool(), iid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_three_findings_when_list_by_issue_then_oldest_id_first() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let a = findings::add(db.pool(), new_finding(iid), TS).await?;
    let b = findings::add(db.pool(), new_finding(iid), TS).await?;
    let c = findings::add(db.pool(), new_finding(iid), TS).await?;
    let ids: Vec<i64> =
        findings::list_by_issue(db.pool(), iid).await?.into_iter().map(|f| f.id).collect();
    assert_eq!(ids, vec![a, b, c]);
    Ok(())
}

#[tokio::test]
async fn given_mixed_status_findings_when_list_open_then_only_open() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let open = findings::add(db.pool(), new_finding(iid), TS).await?;
    let accepted = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::accept(db.pool(), accepted, "will fix", TS2).await?;
    let ids: Vec<i64> =
        findings::list_open(db.pool(), iid).await?.into_iter().map(|f| f.id).collect();
    assert_eq!(ids, vec![open]);
    Ok(())
}

// --- accept ----------------------------------------------------------------

#[tokio::test]
async fn given_open_finding_when_accept_then_status_accepted() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::accept(db.pool(), fid, "will fix", TS2).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.status, FindingStatus::Accepted);
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_accept_then_adjudication_is_rationale() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::accept(db.pool(), fid, "will fix", TS2).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.adjudication.as_deref(), Some("will fix"));
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_accept_then_resolved_at_is_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::accept(db.pool(), fid, "will fix", TS2).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.resolved_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_missing_finding_when_accept_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = findings::accept(db.pool(), 999_999, "x", TS).await;
    assert!(res.is_err(), "accept on missing id must Err");
    Ok(())
}

// --- reject ----------------------------------------------------------------

#[tokio::test]
async fn given_open_finding_when_reject_then_status_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::reject(db.pool(), fid, "false positive", TS2).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.status, FindingStatus::Rejected);
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_reject_then_adjudication_is_rationale() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::reject(db.pool(), fid, "false positive", TS2).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.adjudication.as_deref(), Some("false positive"));
    Ok(())
}

#[tokio::test]
async fn given_missing_finding_when_reject_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = findings::reject(db.pool(), 999_999, "x", TS).await;
    assert!(res.is_err(), "reject on missing id must Err");
    Ok(())
}

// --- dismiss ---------------------------------------------------------------

#[tokio::test]
async fn given_open_finding_when_dismiss_then_status_dismissed() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::dismiss(db.pool(), fid, TS2).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.status, FindingStatus::Dismissed);
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_dismiss_then_adjudication_stays_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::dismiss(db.pool(), fid, TS2).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.adjudication, None);
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_dismiss_then_resolved_at_is_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Review).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::dismiss(db.pool(), fid, TS2).await?;
    let f = findings::get(db.pool(), fid).await?.expect("finding exists");
    assert_eq!(f.resolved_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_missing_finding_when_dismiss_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = findings::dismiss(db.pool(), 999_999, TS).await;
    assert!(res.is_err(), "dismiss on missing id must Err");
    Ok(())
}

// --- Severity / FindingStatus enum round-trips -----------------------------

#[tokio::test]
async fn given_severity_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for v in [Severity::Blocker, Severity::Major, Severity::Minor, Severity::Nit] {
        assert_eq!(Severity::from_str(v.as_str()), Some(v), "{v:?}");
    }
    Ok(())
}

#[tokio::test]
async fn given_severity_as_str_when_checked_then_matches_spec_ids() -> anyhow::Result<()> {
    assert_eq!(Severity::Blocker.as_str(), "blocker");
    assert_eq!(Severity::Major.as_str(), "major");
    assert_eq!(Severity::Minor.as_str(), "minor");
    assert_eq!(Severity::Nit.as_str(), "nit");
    Ok(())
}

#[tokio::test]
async fn given_bogus_or_empty_when_severity_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(Severity::from_str("bogus"), None);
    assert_eq!(Severity::from_str(""), None);
    Ok(())
}

#[tokio::test]
async fn given_finding_status_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for v in [
        FindingStatus::Open,
        FindingStatus::Accepted,
        FindingStatus::Rejected,
        FindingStatus::Dismissed,
    ] {
        assert_eq!(FindingStatus::from_str(v.as_str()), Some(v), "{v:?}");
    }
    Ok(())
}

#[tokio::test]
async fn given_finding_status_as_str_when_checked_then_matches_spec_ids() -> anyhow::Result<()> {
    assert_eq!(FindingStatus::Open.as_str(), "open");
    assert_eq!(FindingStatus::Accepted.as_str(), "accepted");
    assert_eq!(FindingStatus::Rejected.as_str(), "rejected");
    assert_eq!(FindingStatus::Dismissed.as_str(), "dismissed");
    Ok(())
}

#[tokio::test]
async fn given_bogus_or_empty_when_finding_status_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(FindingStatus::from_str("bogus"), None);
    assert_eq!(FindingStatus::from_str(""), None);
    Ok(())
}

// ===========================================================================
// subtasks
// ===========================================================================

#[tokio::test]
async fn given_new_subtask_when_added_then_done_false() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    subtasks::add(db.pool(), iid, 0, "step one", TS).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert!(!st.done);
    Ok(())
}

#[tokio::test]
async fn given_new_subtask_when_added_then_done_at_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    subtasks::add(db.pool(), iid, 0, "step one", TS).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert_eq!(st.done_at, None);
    Ok(())
}

#[tokio::test]
async fn given_new_subtask_when_added_then_text_roundtrips() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    subtasks::add(db.pool(), iid, 0, "step one", TS).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert_eq!(st.text, "step one");
    Ok(())
}

#[tokio::test]
async fn given_no_subtasks_when_list_by_issue_then_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    assert!(subtasks::list_by_issue(db.pool(), iid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_subtasks_with_distinct_ord_when_list_then_ordered_by_ord_asc(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    // Insert out of order; list must sort by ord.
    subtasks::add(db.pool(), iid, 2, "third", TS).await?;
    subtasks::add(db.pool(), iid, 0, "first", TS).await?;
    subtasks::add(db.pool(), iid, 1, "second", TS).await?;
    let texts: Vec<String> =
        subtasks::list_by_issue(db.pool(), iid).await?.into_iter().map(|s| s.text).collect();
    assert_eq!(texts, vec!["first", "second", "third"]);
    Ok(())
}

#[tokio::test]
async fn given_equal_ord_subtasks_when_list_then_tiebreak_by_id_asc() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let a = subtasks::add(db.pool(), iid, 0, "a", TS).await?;
    let b = subtasks::add(db.pool(), iid, 0, "b", TS).await?;
    let ids: Vec<i64> =
        subtasks::list_by_issue(db.pool(), iid).await?.into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![a, b]);
    Ok(())
}

#[tokio::test]
async fn given_subtask_when_mark_done_then_done_true() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = subtasks::add(db.pool(), iid, 0, "x", TS).await?;
    subtasks::mark_done(db.pool(), sid, TS2).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert!(st.done);
    Ok(())
}

#[tokio::test]
async fn given_subtask_when_mark_done_then_done_at_is_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = subtasks::add(db.pool(), iid, 0, "x", TS).await?;
    subtasks::mark_done(db.pool(), sid, TS2).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert_eq!(st.done_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_missing_subtask_when_mark_done_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = subtasks::mark_done(db.pool(), 999_999, TS).await;
    assert!(res.is_err(), "mark_done on missing id must Err");
    Ok(())
}

#[tokio::test]
async fn given_done_subtask_when_mark_undone_then_done_false() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = subtasks::add(db.pool(), iid, 0, "x", TS).await?;
    subtasks::mark_done(db.pool(), sid, TS2).await?;
    subtasks::mark_undone(db.pool(), sid).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert!(!st.done);
    Ok(())
}

#[tokio::test]
async fn given_done_subtask_when_mark_undone_then_done_at_cleared() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = subtasks::add(db.pool(), iid, 0, "x", TS).await?;
    subtasks::mark_done(db.pool(), sid, TS2).await?;
    subtasks::mark_undone(db.pool(), sid).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert_eq!(st.done_at, None);
    Ok(())
}

#[tokio::test]
async fn given_missing_subtask_when_mark_undone_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = subtasks::mark_undone(db.pool(), 999_999).await;
    assert!(res.is_err(), "mark_undone on missing id must Err");
    Ok(())
}

// ===========================================================================
// backlog
// ===========================================================================

#[tokio::test]
async fn given_human_source_when_add_then_approval_approved() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "do x", Source::Human, None, TS).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.approval, Approval::Approved);
    Ok(())
}

#[tokio::test]
async fn given_inbox_source_when_add_then_approval_approved() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "do x", Source::Inbox, None, TS).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.approval, Approval::Approved);
    Ok(())
}

#[tokio::test]
async fn given_agent_source_when_add_then_approval_pending() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "do x", Source::Agent, None, TS).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.approval, Approval::Pending);
    Ok(())
}

#[tokio::test]
async fn given_routine_source_when_add_then_approval_pending() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "do x", Source::Routine, None, TS).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.approval, Approval::Pending);
    Ok(())
}

#[tokio::test]
async fn given_origin_routine_id_when_add_then_roundtrips() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let rid = insert_routine(db.pool(), pid).await?;
    let id = backlog::add(db.pool(), pid, "do x", Source::Routine, Some(rid), TS).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.origin_routine_id, Some(rid));
    Ok(())
}

#[tokio::test]
async fn given_new_backlog_item_when_get_then_consumed_issue_id_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "do x", Source::Human, None, TS).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.consumed_issue_id, None);
    Ok(())
}

#[tokio::test]
async fn given_no_backlog_item_with_id_when_get_then_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    assert!(backlog::get(db.pool(), 999_999).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn given_no_items_when_list_by_project_then_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    assert!(backlog::list_by_project(db.pool(), pid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_three_items_when_list_by_project_then_newest_id_first() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let a = backlog::add(db.pool(), pid, "a", Source::Human, None, TS).await?;
    let b = backlog::add(db.pool(), pid, "b", Source::Human, None, TS).await?;
    let c = backlog::add(db.pool(), pid, "c", Source::Human, None, TS).await?;
    let ids: Vec<i64> =
        backlog::list_by_project(db.pool(), pid).await?.into_iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![c, b, a]);
    Ok(())
}

#[tokio::test]
async fn given_mixed_approval_items_when_list_by_approval_then_only_that_approval(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let approved = backlog::add(db.pool(), pid, "a", Source::Human, None, TS).await?;
    backlog::add(db.pool(), pid, "b", Source::Agent, None, TS).await?; // pending
    let ids: Vec<i64> = backlog::list_by_approval(db.pool(), pid, Approval::Approved)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, vec![approved]);
    Ok(())
}

#[tokio::test]
async fn given_pending_item_when_approve_then_approval_approved() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "x", Source::Agent, None, TS).await?;
    backlog::approve(db.pool(), id, TS2).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.approval, Approval::Approved);
    Ok(())
}

#[tokio::test]
async fn given_pending_item_when_approve_then_resolved_at_is_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "x", Source::Agent, None, TS).await?;
    backlog::approve(db.pool(), id, TS2).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.resolved_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_missing_item_when_approve_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = backlog::approve(db.pool(), 999_999, TS).await;
    assert!(res.is_err(), "approve on missing id must Err");
    Ok(())
}

#[tokio::test]
async fn given_pending_item_when_dismiss_then_approval_dismissed() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "x", Source::Agent, None, TS).await?;
    backlog::dismiss(db.pool(), id, TS2).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.approval, Approval::Dismissed);
    Ok(())
}

#[tokio::test]
async fn given_pending_item_when_dismiss_then_resolved_at_is_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "x", Source::Agent, None, TS).await?;
    backlog::dismiss(db.pool(), id, TS2).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.resolved_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_missing_item_when_dismiss_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = backlog::dismiss(db.pool(), 999_999, TS).await;
    assert!(res.is_err(), "dismiss on missing id must Err");
    Ok(())
}

#[tokio::test]
async fn given_item_when_edit_text_then_text_changes() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "old", Source::Human, None, TS).await?;
    backlog::edit_text(db.pool(), id, "new").await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.text, "new");
    Ok(())
}

#[tokio::test]
async fn given_missing_item_when_edit_text_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = backlog::edit_text(db.pool(), 999_999, "x").await;
    assert!(res.is_err(), "edit_text on missing id must Err");
    Ok(())
}

#[tokio::test]
async fn given_item_when_mark_consumed_then_consumed_issue_id_set() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = issues::create(db.pool(), pid, "t", None, TS).await?;
    let id = backlog::add(db.pool(), pid, "x", Source::Human, None, TS).await?;
    backlog::mark_consumed(db.pool(), id, iid, TS2).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.consumed_issue_id, Some(iid));
    Ok(())
}

#[tokio::test]
async fn given_item_when_mark_consumed_then_resolved_at_is_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = issues::create(db.pool(), pid, "t", None, TS).await?;
    let id = backlog::add(db.pool(), pid, "x", Source::Human, None, TS).await?;
    backlog::mark_consumed(db.pool(), id, iid, TS2).await?;
    let item = backlog::get(db.pool(), id).await?.expect("item exists");
    assert_eq!(item.resolved_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_missing_item_when_mark_consumed_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = backlog::mark_consumed(db.pool(), 999_999, 1, TS).await;
    assert!(res.is_err(), "mark_consumed on missing id must Err");
    Ok(())
}

#[tokio::test]
async fn given_item_when_remove_then_get_returns_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = backlog::add(db.pool(), pid, "x", Source::Human, None, TS).await?;
    backlog::remove(db.pool(), id).await?;
    assert!(backlog::get(db.pool(), id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn given_missing_item_when_remove_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = backlog::remove(db.pool(), 999_999).await;
    assert!(res.is_err(), "remove on missing id must Err");
    Ok(())
}

// --- default_approval per source -------------------------------------------

#[tokio::test]
async fn given_human_source_when_default_approval_then_approved() -> anyhow::Result<()> {
    assert_eq!(Source::Human.default_approval(), Approval::Approved);
    Ok(())
}

#[tokio::test]
async fn given_inbox_source_when_default_approval_then_approved() -> anyhow::Result<()> {
    assert_eq!(Source::Inbox.default_approval(), Approval::Approved);
    Ok(())
}

#[tokio::test]
async fn given_agent_source_when_default_approval_then_pending() -> anyhow::Result<()> {
    assert_eq!(Source::Agent.default_approval(), Approval::Pending);
    Ok(())
}

#[tokio::test]
async fn given_routine_source_when_default_approval_then_pending() -> anyhow::Result<()> {
    assert_eq!(Source::Routine.default_approval(), Approval::Pending);
    Ok(())
}

// --- Source / Approval enum round-trips ------------------------------------

#[tokio::test]
async fn given_source_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for v in [Source::Human, Source::Agent, Source::Routine, Source::Inbox] {
        assert_eq!(Source::from_str(v.as_str()), Some(v), "{v:?}");
    }
    Ok(())
}

#[tokio::test]
async fn given_source_as_str_when_checked_then_matches_spec_ids() -> anyhow::Result<()> {
    assert_eq!(Source::Human.as_str(), "human");
    assert_eq!(Source::Agent.as_str(), "agent");
    assert_eq!(Source::Routine.as_str(), "routine");
    assert_eq!(Source::Inbox.as_str(), "inbox");
    Ok(())
}

#[tokio::test]
async fn given_bogus_or_empty_when_source_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(Source::from_str("bogus"), None);
    assert_eq!(Source::from_str(""), None);
    Ok(())
}

#[tokio::test]
async fn given_approval_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for v in [Approval::Pending, Approval::Approved, Approval::Dismissed] {
        assert_eq!(Approval::from_str(v.as_str()), Some(v), "{v:?}");
    }
    Ok(())
}

#[tokio::test]
async fn given_approval_as_str_when_checked_then_matches_spec_ids() -> anyhow::Result<()> {
    assert_eq!(Approval::Pending.as_str(), "pending");
    assert_eq!(Approval::Approved.as_str(), "approved");
    assert_eq!(Approval::Dismissed.as_str(), "dismissed");
    Ok(())
}

#[tokio::test]
async fn given_bogus_or_empty_when_approval_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(Approval::from_str("bogus"), None);
    assert_eq!(Approval::from_str(""), None);
    Ok(())
}

// ===========================================================================
// steering
// ===========================================================================

#[tokio::test]
async fn given_working_phase_issue_when_steering_added_then_get_returns_note(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await?;
    let s = steering::get(db.pool(), sid).await?.expect("steering exists");
    assert_eq!(s.note, "go left");
    Ok(())
}

#[tokio::test]
async fn given_new_steering_when_added_then_not_consumed() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await?;
    let s = steering::get(db.pool(), sid).await?.expect("steering exists");
    assert!(!s.consumed);
    Ok(())
}

#[tokio::test]
async fn given_steering_added_when_issue_read_then_has_pending_steering_true(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await?;
    let issue = issues::get(db.pool(), iid).await?.expect("issue exists");
    assert!(issue.has_pending_steering, "add must flip issue.has_pending_steering");
    Ok(())
}

#[tokio::test]
async fn given_planned_issue_when_steering_added_then_err() -> anyhow::Result<()> {
    // Structurally valid but semantically wrong: PLANNED is not a working phase,
    // so the steering guard must reject it.
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Planned).await?;
    let res = steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await;
    assert!(res.is_err(), "steering into a PLANNED issue must Err");
    Ok(())
}

#[tokio::test]
async fn given_planned_issue_when_steering_add_fails_then_flag_stays_false(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Planned).await?;
    let _ = steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await;
    let issue = issues::get(db.pool(), iid).await?.expect("issue exists");
    assert!(!issue.has_pending_steering, "rejected steering must not flip the flag");
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_steering_added_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = steering::add(db.pool(), 999_999, SteeringSource::Human, "x", TS).await;
    assert!(res.is_err(), "steering into a missing issue must Err");
    Ok(())
}

#[tokio::test]
async fn given_no_steering_with_id_when_get_then_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    assert!(steering::get(db.pool(), 999_999).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn given_no_steering_when_list_pending_then_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    assert!(steering::list_pending(db.pool(), iid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_multiple_pending_steering_when_list_pending_then_oldest_id_first(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let a = steering::add(db.pool(), iid, SteeringSource::Human, "first", TS).await?;
    let b = steering::add(db.pool(), iid, SteeringSource::Human, "second", TS).await?;
    let ids: Vec<i64> =
        steering::list_pending(db.pool(), iid).await?.into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![a, b]);
    Ok(())
}

#[tokio::test]
async fn given_pending_steering_when_consume_all_then_list_pending_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::add(db.pool(), iid, SteeringSource::Human, "b", TS).await?;
    steering::consume_all(db.pool(), iid, TS2).await?;
    assert!(steering::list_pending(db.pool(), iid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_consumed_steering_when_get_then_consumed_at_is_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::consume_all(db.pool(), iid, TS2).await?;
    let s = steering::get(db.pool(), sid).await?.expect("steering exists");
    assert_eq!(s.consumed_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_consume_all_when_done_then_issue_flag_cleared() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::consume_all(db.pool(), iid, TS2).await?;
    let issue = issues::get(db.pool(), iid).await?.expect("issue exists");
    assert!(!issue.has_pending_steering, "consume_all must clear the flag");
    Ok(())
}

#[tokio::test]
async fn given_pending_steering_when_remove_pending_then_list_pending_empty(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::remove_pending(db.pool(), sid).await?;
    assert!(steering::list_pending(db.pool(), iid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_already_consumed_steering_when_remove_pending_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::consume_all(db.pool(), iid, TS2).await?;
    let res = steering::remove_pending(db.pool(), sid).await;
    assert!(res.is_err(), "remove_pending on a consumed note must Err");
    Ok(())
}

#[tokio::test]
async fn given_missing_steering_when_remove_pending_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = steering::remove_pending(db.pool(), 999_999).await;
    assert!(res.is_err(), "remove_pending on missing id must Err");
    Ok(())
}

// --- SteeringSource enum round-trip ----------------------------------------

#[tokio::test]
async fn given_steering_source_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for v in [SteeringSource::Human, SteeringSource::Consolidation] {
        assert_eq!(SteeringSource::from_str(v.as_str()), Some(v), "{v:?}");
    }
    Ok(())
}

#[tokio::test]
async fn given_steering_source_as_str_when_checked_then_matches_spec_ids() -> anyhow::Result<()> {
    assert_eq!(SteeringSource::Human.as_str(), "human");
    assert_eq!(SteeringSource::Consolidation.as_str(), "consolidation");
    Ok(())
}

#[tokio::test]
async fn given_bogus_or_empty_when_steering_source_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(SteeringSource::from_str("bogus"), None);
    assert_eq!(SteeringSource::from_str(""), None);
    Ok(())
}

// --- extra guard / idempotency / FK coverage -------------------------------

#[tokio::test]
async fn given_audit_issue_when_steering_added_then_get_returns_some() -> anyhow::Result<()> {
    // AUDIT is a working phase, so the real DB guard must accept steering.
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Audit).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "recheck", TS).await?;
    assert!(steering::get(db.pool(), sid).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn given_done_issue_when_steering_added_then_err() -> anyhow::Result<()> {
    // DONE is terminal and non-working, so the steering guard must reject it.
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Done).await?;
    let res = steering::add(db.pool(), iid, SteeringSource::Human, "recheck", TS).await;
    assert!(res.is_err(), "steering into a DONE issue must Err");
    Ok(())
}

#[tokio::test]
async fn given_consumed_steering_when_consume_all_again_then_consumed_at_not_restamped(
) -> anyhow::Result<()> {
    // A second consume_all at a later clock must not re-stamp an already-consumed note.
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Implementing).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::consume_all(db.pool(), iid, TS2).await?;
    steering::consume_all(db.pool(), iid, TS2 + 1).await?;
    let s = steering::get(db.pool(), sid).await?.expect("steering exists");
    assert_eq!(s.consumed_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_dangling_issue_id_when_finding_added_then_err() -> anyhow::Result<()> {
    // No issue with id 999_999 exists, so the FK on findings.issue_id must reject.
    let db = Db::open_memory().await?;
    let res = findings::add(db.pool(), new_finding(999_999), TS).await;
    assert!(res.is_err(), "findings::add with a dangling issue_id must Err");
    Ok(())
}

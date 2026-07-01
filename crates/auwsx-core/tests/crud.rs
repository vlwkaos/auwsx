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
use auwsx_core::db::arsenal::{self, NewArsenalPreset};
use auwsx_core::db::ask_answers::{self, AskMode};
use auwsx_core::db::findings::{self, FindingStatus, NewFinding, Severity};
use auwsx_core::db::global_settings::{self, PIPELINE_UX_GUIDANCE_MAX_CHARS};
use auwsx_core::db::issues::{self, INITIAL_STATUS};
use auwsx_core::db::profiles;
use auwsx_core::db::projects::{self, CompletionPolicy, MergeMode, NewProject};
use auwsx_core::db::subtasks;
use auwsx_core::db::Db;
use auwsx_core::main_jobs::{self, MainJobStatus};
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
            arsenal_preset_name: None,
            main_agent_cmd: "claude {prompt}",
            route_agent_cmd: "claude {prompt}",
            plan_agent_cmd: "claude-plan {prompt}",
            work_agent_cmd: "claude-work {prompt}",
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
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

async fn insert_main_job(pool: &SqlitePool, project_id: i64) -> anyhow::Result<i64> {
    let routine_id = insert_routine(pool, project_id).await?;
    main_jobs::enqueue_routine(pool, project_id, routine_id, "report", "prompt", TS).await
}

async fn upsert_preset(pool: &SqlitePool, name: &str) -> anyhow::Result<i64> {
    arsenal::upsert(
        pool,
        NewArsenalPreset {
            name,
            main_agent_cmd: "main {prompt}",
            route_agent_cmd: "main {prompt}",
            plan_agent_cmd: "plan {prompt}",
            work_agent_cmd: "work {prompt}",
            review_agent_cmd: Some("review {prompt}"),
        },
        TS,
    )
    .await
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
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: Some("reviewer {prompt}"),
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.review_agent_cmd.as_deref(), Some("reviewer {prompt}"));
    Ok(())
}

#[tokio::test]
async fn given_project_with_arsenal_and_blank_overrides_when_get_then_effective_commands_resolve(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    upsert_preset(db.pool(), "local").await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "alpha",
            repo_path: "/r",
            default_branch: "main",
            arsenal_preset_name: Some("local"),
            main_agent_cmd: "",
            route_agent_cmd: "",
            plan_agent_cmd: "",
            work_agent_cmd: "",
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;

    let p = projects::get(db.pool(), id).await?.expect("project exists");

    assert_eq!(p.arsenal_preset_name.as_deref(), Some("local"));
    assert_eq!(p.main_agent_cmd, "main {prompt}");
    assert_eq!(p.plan_agent_cmd, "plan {prompt}");
    assert_eq!(p.work_agent_cmd, "work {prompt}");
    assert_eq!(p.review_agent_cmd.as_deref(), Some("review {prompt}"));
    assert_eq!(p.main_agent_cmd_override, None);
    assert_eq!(p.plan_agent_cmd_override, None);
    assert_eq!(p.work_agent_cmd_override, None);
    Ok(())
}

#[tokio::test]
async fn given_linked_project_when_arsenal_updates_then_effective_commands_follow(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    upsert_preset(db.pool(), "local").await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "alpha",
            repo_path: "/r",
            default_branch: "main",
            arsenal_preset_name: Some("local"),
            main_agent_cmd: "",
            route_agent_cmd: "",
            plan_agent_cmd: "",
            work_agent_cmd: "",
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    arsenal::upsert(
        db.pool(),
        NewArsenalPreset {
            name: "local",
            main_agent_cmd: "main2 {prompt}",
            route_agent_cmd: "main2 {prompt}",
            plan_agent_cmd: "plan2 {prompt}",
            work_agent_cmd: "work2 {prompt}",
            review_agent_cmd: Some("review2 {prompt}"),
        },
        TS + 1,
    )
    .await?;

    let p = projects::get(db.pool(), id).await?.expect("project exists");

    assert_eq!(p.main_agent_cmd, "main2 {prompt}");
    assert_eq!(p.plan_agent_cmd, "plan2 {prompt}");
    assert_eq!(p.work_agent_cmd, "work2 {prompt}");
    assert_eq!(p.review_agent_cmd.as_deref(), Some("review2 {prompt}"));
    Ok(())
}

#[tokio::test]
async fn given_project_with_arsenal_and_manual_override_when_get_then_override_wins(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    upsert_preset(db.pool(), "local").await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "alpha",
            repo_path: "/r",
            default_branch: "main",
            arsenal_preset_name: Some("local"),
            main_agent_cmd: "manual-main {prompt}",
            route_agent_cmd: "manual-main {prompt}",
            plan_agent_cmd: "",
            work_agent_cmd: "",
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;

    let p = projects::get(db.pool(), id).await?.expect("project exists");

    assert_eq!(p.main_agent_cmd, "manual-main {prompt}");
    assert_eq!(
        p.main_agent_cmd_override.as_deref(),
        Some("manual-main {prompt}")
    );
    assert_eq!(p.plan_agent_cmd, "plan {prompt}");
    assert_eq!(p.plan_agent_cmd_override, None);
    Ok(())
}

#[tokio::test]
async fn given_unknown_arsenal_when_project_created_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let err = projects::create(
        db.pool(),
        NewProject {
            name: "alpha",
            repo_path: "/r",
            default_branch: "main",
            arsenal_preset_name: Some("missing"),
            main_agent_cmd: "",
            route_agent_cmd: "",
            plan_agent_cmd: "",
            work_agent_cmd: "",
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await
    .expect_err("unknown Arsenal preset must fail");

    assert!(
        err.to_string().contains("unknown Arsenal preset"),
        "unexpected error: {err:#}"
    );
    Ok(())
}

// --- SQL DEFAULTs after a minimal create -----------------------------------

#[tokio::test]
async fn given_minimal_create_when_get_then_completion_policy_defaults_manual() -> anyhow::Result<()>
{
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
async fn given_minimal_create_when_get_then_max_concurrency_defaults_3() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "p").await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.max_concurrency, 3);
    Ok(())
}

#[tokio::test]
async fn given_minimal_create_when_get_then_schedule_interval_defaults_none() -> anyhow::Result<()>
{
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
async fn given_minimal_create_when_get_then_last_deepsleep_at_defaults_none() -> anyhow::Result<()>
{
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
    let by_name = projects::get_by_name(db.pool(), "alpha")
        .await?
        .expect("by name");
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
    let names: Vec<String> = projects::list(db.pool())
        .await?
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, vec!["a", "b", "c"]);
    Ok(())
}

#[tokio::test]
async fn given_projects_created_without_profile_when_get_then_default_profile_and_order_assigned(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let a = insert_project(db.pool(), "a").await?;
    let b = insert_project(db.pool(), "b").await?;
    let pa = projects::get(db.pool(), a)
        .await?
        .expect("project a exists");
    let pb = projects::get(db.pool(), b)
        .await?
        .expect("project b exists");
    assert_eq!(
        (
            pa.profile_id,
            pa.profile_order,
            pb.profile_id,
            pb.profile_order
        ),
        (1, 1, 1, 2)
    );
    Ok(())
}

#[tokio::test]
async fn given_project_moved_to_profile_when_get_then_appended_to_that_profile(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let moved = insert_project(db.pool(), "moved").await?;
    let other_existing = insert_project(db.pool(), "other-existing").await?;
    let profile_id = profiles::create(db.pool(), "custom", TS).await?;
    projects::move_to_profile(db.pool(), other_existing, profile_id).await?;

    projects::move_to_profile(db.pool(), moved, profile_id).await?;

    let p = projects::get(db.pool(), moved)
        .await?
        .expect("project exists");
    assert_eq!((p.profile_id, p.profile_order), (profile_id, 2));
    Ok(())
}

#[tokio::test]
async fn given_missing_project_when_move_to_profile_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let profile_id = profiles::create(db.pool(), "custom", TS).await?;
    let res = projects::move_to_profile(db.pool(), 999_999, profile_id).await;
    assert!(res.is_err(), "moving a missing project must Err");
    Ok(())
}

#[tokio::test]
async fn given_missing_profile_when_move_project_to_profile_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;
    let res = projects::move_to_profile(db.pool(), project_id, 999_999).await;
    assert!(res.is_err(), "moving to a missing profile must Err");
    Ok(())
}

#[tokio::test]
async fn given_project_moved_past_profile_start_when_list_then_clamped_within_profile(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    insert_project(db.pool(), "a").await?;
    insert_project(db.pool(), "b").await?;
    let c = insert_project(db.pool(), "c").await?;

    projects::move_within_profile(db.pool(), c, -99).await?;

    let names: Vec<String> = projects::list(db.pool())
        .await?
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, vec!["c", "b", "a"]);
    Ok(())
}

#[tokio::test]
async fn given_projects_in_other_profile_when_moving_default_profile_then_other_profile_unchanged(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let a = insert_project(db.pool(), "a").await?;
    let b = insert_project(db.pool(), "b").await?;
    let x = insert_project(db.pool(), "x").await?;
    let y = insert_project(db.pool(), "y").await?;
    let profile_id = profiles::create(db.pool(), "custom", TS).await?;
    projects::move_to_profile(db.pool(), x, profile_id).await?;
    projects::move_to_profile(db.pool(), y, profile_id).await?;

    projects::move_within_profile(db.pool(), b, -1).await?;

    let rows = projects::list(db.pool()).await?;
    let names: Vec<String> = rows.into_iter().map(|p| p.name).collect();
    assert_eq!(names, vec!["b", "a", "x", "y"]);
    let a_row = projects::get(db.pool(), a).await?.expect("a exists");
    assert_eq!(a_row.profile_order, 2);
    Ok(())
}

#[tokio::test]
async fn given_missing_project_when_move_within_profile_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = projects::move_within_profile(db.pool(), 999_999, 1).await;
    assert!(res.is_err(), "moving a missing project must Err");
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
    for v in [
        CompletionPolicy::Manual,
        CompletionPolicy::Soft,
        CompletionPolicy::Auto,
    ] {
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
async fn given_new_issue_when_created_then_status_is_initial_new() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.status, INITIAL_STATUS);
    Ok(())
}

#[tokio::test]
async fn given_initial_status_const_when_checked_then_equals_new() -> anyhow::Result<()> {
    assert_eq!(INITIAL_STATUS, IssueStatus::New);
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
async fn given_new_issue_without_description_when_get_then_description_none() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.description, None);
    Ok(())
}

#[tokio::test]
async fn given_new_issue_when_created_then_counters_and_flags_default() -> anyhow::Result<()> {
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
    let ids: Vec<i64> = issues::list_by_project(db.pool(), pid)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, vec![c, b, a]);
    Ok(())
}

#[tokio::test]
async fn given_issues_in_other_project_when_list_by_project_then_excluded() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let p1 = insert_project(db.pool(), "p1").await?;
    let p2 = insert_project(db.pool(), "p2").await?;
    issues::create(db.pool(), p2, "other", None, TS).await?;
    let mine = issues::create(db.pool(), p1, "mine", None, TS).await?;
    let ids: Vec<i64> = issues::list_by_project(db.pool(), p1)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, vec![mine]);
    Ok(())
}

#[tokio::test]
async fn given_issues_of_mixed_status_when_list_by_status_then_only_that_status(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let planning = insert_issue_at(db.pool(), pid, IssueStatus::Planning).await?;
    insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
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
    insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let got = issues::list_by_status(db.pool(), pid, IssueStatus::Done).await?;
    assert!(got.is_empty());
    Ok(())
}

// --- transition: legal, illegal, timestamp ---------------------------------

#[tokio::test]
async fn given_new_when_transition_to_planning_then_status_planning() -> anyhow::Result<()> {
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
async fn given_new_when_transition_to_done_then_err() -> anyhow::Result<()> {
    // Structurally valid but semantically wrong: NEW -> DONE is illegal.
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let res = issues::transition(db.pool(), id, IssueStatus::Done, TS2).await;
    assert!(res.is_err(), "NEW -> DONE must be rejected");
    Ok(())
}

#[tokio::test]
async fn given_illegal_transition_when_attempted_then_status_unchanged() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let _ = issues::transition(db.pool(), id, IssueStatus::Working, TS2).await;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(issue.status, IssueStatus::New);
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
    // NEW -> DONE is illegal for transition, but force bypasses the matrix.
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

// --- mark_absorbed compatibility guard ------------------------------------

#[tokio::test]
async fn given_issue_when_mark_absorbed_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let donor = issues::create(db.pool(), pid, "donor", None, TS).await?;
    let target = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let res = issues::mark_absorbed(db.pool(), donor, target, TS2).await;
    assert!(
        res.is_err(),
        "direct issue absorption is no longer supported"
    );
    Ok(())
}

#[tokio::test]
async fn given_issue_when_mark_absorbed_err_then_issue_is_unchanged() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let donor = issues::create(db.pool(), pid, "donor", None, TS).await?;
    let target = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let res = issues::mark_absorbed(db.pool(), donor, target, TS2).await;
    assert!(
        res.is_err(),
        "direct issue absorption is no longer supported"
    );
    let issue = issues::get(db.pool(), donor).await?.expect("issue exists");
    assert_eq!(issue.status, IssueStatus::New);
    assert_eq!(issue.absorbed_into_id, None);
    Ok(())
}

#[tokio::test]
async fn given_absorb_target_not_working_when_mark_absorbed_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let donor = issues::create(db.pool(), pid, "donor", None, TS).await?;
    let target = issues::create(db.pool(), pid, "target", None, TS).await?;
    let res = issues::mark_absorbed(db.pool(), donor, target, TS2).await;
    assert!(res.is_err(), "target must accept steering");
    Ok(())
}

#[tokio::test]
async fn given_absorb_target_in_other_project_when_mark_absorbed_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let other_pid = insert_project(db.pool(), "other").await?;
    let donor = issues::create(db.pool(), pid, "donor", None, TS).await?;
    let target = insert_issue_at(db.pool(), other_pid, IssueStatus::Working).await?;
    let res = issues::mark_absorbed(db.pool(), donor, target, TS2).await;
    assert!(res.is_err(), "target must belong to the same project");
    Ok(())
}

#[tokio::test]
async fn given_non_new_issue_when_mark_absorbed_then_err() -> anyhow::Result<()> {
    // ->ABANDONED is legal only from NEW; from PLANNING it must Err.
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
    issues::set_worktree(db.pool(), id, Some("br"), Some("/wt"), TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!(
        (issue.branch.as_deref(), issue.worktree_path.as_deref()),
        (Some("br"), Some("/wt"))
    );
    Ok(())
}

#[tokio::test]
async fn given_issue_when_set_worktree_all_none_then_fields_null() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    issues::set_worktree(db.pool(), id, None, None, TS2).await?;
    let issue = issues::get(db.pool(), id).await?.expect("issue exists");
    assert_eq!((issue.branch, issue.worktree_path), (None, None));
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_set_worktree_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = issues::set_worktree(db.pool(), 999_999, None, None, TS).await;
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
async fn given_bumped_review_round_when_get_then_persisted_value_matches() -> anyhow::Result<()> {
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
    assert_eq!(
        new, 1,
        "conflict_attempts starts at 0; first bump returns 1"
    );
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_bump_conflict_attempts_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = issues::bump_conflict_attempts(db.pool(), 999_999, TS).await;
    assert!(
        res.is_err(),
        "bump_conflict_attempts on missing id must Err"
    );
    Ok(())
}

// --- IssueStatus::accepts_queue_message set --------------------------------

#[tokio::test]
async fn given_queue_eligible_statuses_when_accepts_queue_message_then_true() -> anyhow::Result<()>
{
    for v in [
        IssueStatus::New,
        IssueStatus::Planning,
        IssueStatus::PlanReady,
        IssueStatus::PlanBlocked,
        IssueStatus::Working,
        IssueStatus::Reviewing,
        IssueStatus::Fixing,
        IssueStatus::Auditing,
        IssueStatus::ReadyToMerge,
    ] {
        assert!(
            v.accepts_queue_message(),
            "{v:?} should accept queue messages"
        );
    }
    Ok(())
}

#[tokio::test]
async fn given_non_queue_eligible_status_when_accepts_queue_message_then_false(
) -> anyhow::Result<()> {
    assert!(!IssueStatus::Merging.accepts_queue_message());
    Ok(())
}

// --- is_legal_transition sample contract -----------------------------------

#[tokio::test]
async fn given_new_to_planning_when_checked_then_legal() -> anyhow::Result<()> {
    assert!(is_legal_transition(IssueStatus::New, IssueStatus::Planning));
    Ok(())
}

#[tokio::test]
async fn given_new_to_working_when_checked_then_illegal() -> anyhow::Result<()> {
    assert!(!is_legal_transition(IssueStatus::New, IssueStatus::Working));
    Ok(())
}

#[tokio::test]
async fn given_ready_to_merge_to_working_when_checked_then_legal() -> anyhow::Result<()> {
    assert!(is_legal_transition(
        IssueStatus::ReadyToMerge,
        IssueStatus::Working
    ));
    Ok(())
}

#[tokio::test]
async fn given_new_to_abandoned_when_checked_then_legal() -> anyhow::Result<()> {
    assert!(is_legal_transition(
        IssueStatus::New,
        IssueStatus::Abandoned
    ));
    Ok(())
}

#[tokio::test]
async fn given_new_to_done_when_checked_then_illegal() -> anyhow::Result<()> {
    assert!(!is_legal_transition(IssueStatus::New, IssueStatus::Done));
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
    assert_eq!(f.status, FindingStatus::Open);
    Ok(())
}

#[tokio::test]
async fn given_new_finding_when_added_then_resolved_at_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
    assert_eq!(f.resolved_at, None);
    Ok(())
}

#[tokio::test]
async fn given_finding_with_optional_fields_when_get_then_roundtrip() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
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
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
    assert_eq!(
        (
            f.review_round,
            f.severity,
            f.lens.as_deref(),
            f.detail.as_deref(),
            f.file_ref.as_deref()
        ),
        (
            2,
            Severity::Blocker,
            Some("security"),
            Some("token logged"),
            Some("src/x.rs:10")
        )
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    assert!(findings::list_by_issue(db.pool(), iid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_three_findings_when_list_by_issue_then_oldest_id_first() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let a = findings::add(db.pool(), new_finding(iid), TS).await?;
    let b = findings::add(db.pool(), new_finding(iid), TS).await?;
    let c = findings::add(db.pool(), new_finding(iid), TS).await?;
    let ids: Vec<i64> = findings::list_by_issue(db.pool(), iid)
        .await?
        .into_iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(ids, vec![a, b, c]);
    Ok(())
}

#[tokio::test]
async fn given_mixed_status_findings_when_list_open_then_only_open() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let open = findings::add(db.pool(), new_finding(iid), TS).await?;
    let accepted = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::accept(db.pool(), accepted, "will fix", TS2).await?;
    let ids: Vec<i64> = findings::list_open(db.pool(), iid)
        .await?
        .into_iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(ids, vec![open]);
    Ok(())
}

// --- accept ----------------------------------------------------------------

#[tokio::test]
async fn given_open_finding_when_accept_then_status_accepted() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::accept(db.pool(), fid, "will fix", TS2).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
    assert_eq!(f.status, FindingStatus::Accepted);
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_accept_then_adjudication_is_rationale() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::accept(db.pool(), fid, "will fix", TS2).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
    assert_eq!(f.adjudication.as_deref(), Some("will fix"));
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_accept_then_resolved_at_is_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::accept(db.pool(), fid, "will fix", TS2).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::reject(db.pool(), fid, "false positive", TS2).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
    assert_eq!(f.status, FindingStatus::Rejected);
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_reject_then_adjudication_is_rationale() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::reject(db.pool(), fid, "false positive", TS2).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::dismiss(db.pool(), fid, TS2).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
    assert_eq!(f.status, FindingStatus::Dismissed);
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_dismiss_then_adjudication_stays_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::dismiss(db.pool(), fid, TS2).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
    assert_eq!(f.adjudication, None);
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_dismiss_then_resolved_at_is_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Reviewing).await?;
    let fid = findings::add(db.pool(), new_finding(iid), TS).await?;
    findings::dismiss(db.pool(), fid, TS2).await?;
    let f = findings::get(db.pool(), fid)
        .await?
        .expect("finding exists");
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
    for v in [
        Severity::Blocker,
        Severity::Major,
        Severity::Minor,
        Severity::Nit,
    ] {
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    subtasks::add(db.pool(), iid, 0, "step one", TS).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert!(!st.done);
    Ok(())
}

#[tokio::test]
async fn given_new_subtask_when_added_then_done_at_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    subtasks::add(db.pool(), iid, 0, "step one", TS).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert_eq!(st.done_at, None);
    Ok(())
}

#[tokio::test]
async fn given_new_subtask_when_added_then_text_roundtrips() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    subtasks::add(db.pool(), iid, 0, "step one", TS).await?;
    let st = &subtasks::list_by_issue(db.pool(), iid).await?[0];
    assert_eq!(st.text, "step one");
    Ok(())
}

#[tokio::test]
async fn given_no_subtasks_when_list_by_issue_then_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    assert!(subtasks::list_by_issue(db.pool(), iid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_subtasks_with_distinct_ord_when_list_then_ordered_by_ord_asc() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    // Insert out of order; list must sort by ord.
    subtasks::add(db.pool(), iid, 2, "third", TS).await?;
    subtasks::add(db.pool(), iid, 0, "first", TS).await?;
    subtasks::add(db.pool(), iid, 1, "second", TS).await?;
    let texts: Vec<String> = subtasks::list_by_issue(db.pool(), iid)
        .await?
        .into_iter()
        .map(|s| s.text)
        .collect();
    assert_eq!(texts, vec!["first", "second", "third"]);
    Ok(())
}

#[tokio::test]
async fn given_equal_ord_subtasks_when_list_then_tiebreak_by_id_asc() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let a = subtasks::add(db.pool(), iid, 0, "a", TS).await?;
    let b = subtasks::add(db.pool(), iid, 0, "b", TS).await?;
    let ids: Vec<i64> = subtasks::list_by_issue(db.pool(), iid)
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec![a, b]);
    Ok(())
}

#[tokio::test]
async fn given_subtask_when_mark_done_then_done_true() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
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
    let ids: Vec<i64> = backlog::list_by_project(db.pool(), pid)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, vec![c, b, a]);
    Ok(())
}

#[tokio::test]
async fn given_consumed_item_when_list_by_project_then_excluded() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let issue_id = issues::create(db.pool(), pid, "promoted", None, TS).await?;
    let consumed = backlog::add(db.pool(), pid, "consumed", Source::Human, None, TS).await?;
    let live = backlog::add(db.pool(), pid, "live", Source::Human, None, TS).await?;
    backlog::mark_consumed(db.pool(), consumed, issue_id, TS2).await?;

    let ids: Vec<i64> = backlog::list_by_project(db.pool(), pid)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();

    assert_eq!(ids, vec![live]);
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
async fn given_consumed_item_when_list_by_approval_then_excluded() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let issue_id = issues::create(db.pool(), pid, "promoted", None, TS).await?;
    let consumed = backlog::add(db.pool(), pid, "consumed", Source::Human, None, TS).await?;
    let live = backlog::add(db.pool(), pid, "live", Source::Human, None, TS).await?;
    backlog::mark_consumed(db.pool(), consumed, issue_id, TS2).await?;

    let ids: Vec<i64> = backlog::list_by_approval(db.pool(), pid, Approval::Approved)
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();

    assert_eq!(ids, vec![live]);
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
async fn given_consumed_item_when_mark_consumed_again_then_original_issue_stays(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let first_issue = issues::create(db.pool(), pid, "first", None, TS).await?;
    let second_issue = issues::create(db.pool(), pid, "second", None, TS).await?;
    let item_id = backlog::add(db.pool(), pid, "x", Source::Human, None, TS).await?;

    backlog::mark_consumed(db.pool(), item_id, first_issue, TS).await?;
    let res = backlog::mark_consumed(db.pool(), item_id, second_issue, TS + 1).await;

    assert!(res.is_err(), "second consumption must lose the race");
    let item = backlog::get(db.pool(), item_id)
        .await?
        .expect("item exists");
    assert_eq!(item.consumed_issue_id, Some(first_issue));
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
async fn given_working_phase_issue_when_steering_added_then_get_returns_note() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await?;
    let s = steering::get(db.pool(), sid)
        .await?
        .expect("steering exists");
    assert_eq!(s.note, "go left");
    Ok(())
}

#[tokio::test]
async fn given_ready_to_merge_issue_when_steering_added_then_get_returns_note() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::ReadyToMerge).await?;
    let sid = steering::add(
        db.pool(),
        iid,
        SteeringSource::Human,
        "verify one more case before merge",
        TS,
    )
    .await?;
    let s = steering::get(db.pool(), sid)
        .await?
        .expect("steering exists");
    assert_eq!(s.note, "verify one more case before merge");
    Ok(())
}

#[tokio::test]
async fn given_new_steering_when_added_then_not_consumed() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await?;
    let s = steering::get(db.pool(), sid)
        .await?
        .expect("steering exists");
    assert!(!s.consumed);
    Ok(())
}

#[tokio::test]
async fn given_steering_added_when_issue_read_then_has_pending_steering_true() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await?;
    let issue = issues::get(db.pool(), iid).await?.expect("issue exists");
    assert!(
        issue.has_pending_steering,
        "add must flip issue.has_pending_steering"
    );
    Ok(())
}

#[tokio::test]
async fn given_plan_ready_issue_when_steering_added_then_get_returns_note() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::PlanReady).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await?;
    let s = steering::get(db.pool(), sid)
        .await?
        .expect("steering exists");
    assert_eq!(s.note, "go left");
    Ok(())
}

#[tokio::test]
async fn given_plan_ready_issue_when_steering_added_then_flag_is_true() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::PlanReady).await?;
    steering::add(db.pool(), iid, SteeringSource::Human, "go left", TS).await?;
    let issue = issues::get(db.pool(), iid).await?.expect("issue exists");
    assert!(
        issue.has_pending_steering,
        "pre-work steering must mark the plan stale"
    );
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    assert!(steering::list_pending(db.pool(), iid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_multiple_pending_steering_when_list_pending_then_oldest_id_first(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let a = steering::add(db.pool(), iid, SteeringSource::Human, "first", TS).await?;
    let b = steering::add(db.pool(), iid, SteeringSource::Human, "second", TS).await?;
    let ids: Vec<i64> = steering::list_pending(db.pool(), iid)
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec![a, b]);
    Ok(())
}

#[tokio::test]
async fn given_pending_steering_when_consume_all_then_list_pending_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::consume_all(db.pool(), iid, TS2).await?;
    let s = steering::get(db.pool(), sid)
        .await?
        .expect("steering exists");
    assert_eq!(s.consumed_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_consume_all_when_done_then_issue_flag_cleared() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::consume_all(db.pool(), iid, TS2).await?;
    let issue = issues::get(db.pool(), iid).await?.expect("issue exists");
    assert!(
        !issue.has_pending_steering,
        "consume_all must clear the flag"
    );
    Ok(())
}

#[tokio::test]
async fn given_pending_steering_when_remove_pending_then_list_pending_empty() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::remove_pending(db.pool(), sid).await?;
    assert!(steering::list_pending(db.pool(), iid).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn given_already_consumed_steering_when_remove_pending_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
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
    // AUDITING is a working phase, so the real DB guard must accept steering.
    let db = Db::open_memory().await?;
    let pid = insert_project(db.pool(), "p").await?;
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Auditing).await?;
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
    let iid = insert_issue_at(db.pool(), pid, IssueStatus::Working).await?;
    let sid = steering::add(db.pool(), iid, SteeringSource::Human, "a", TS).await?;
    steering::consume_all(db.pool(), iid, TS2).await?;
    steering::consume_all(db.pool(), iid, TS2 + 1).await?;
    let s = steering::get(db.pool(), sid)
        .await?
        .expect("steering exists");
    assert_eq!(s.consumed_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_dangling_issue_id_when_finding_added_then_err() -> anyhow::Result<()> {
    // No issue with id 999_999 exists, so the FK on findings.issue_id must reject.
    let db = Db::open_memory().await?;
    let res = findings::add(db.pool(), new_finding(999_999), TS).await;
    assert!(
        res.is_err(),
        "findings::add with a dangling issue_id must Err"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// projects::create — policy overrides (None keeps DB DEFAULT; Some overrides
// only that column via COALESCE, leaving siblings at their defaults).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_completion_policy_some_auto_when_created_then_completion_policy_is_auto(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_auto_policy",
            repo_path: "/repo",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Auto),
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let row = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(row.completion_policy, CompletionPolicy::Auto);
    Ok(())
}

#[tokio::test]
async fn given_completion_policy_some_auto_when_created_then_plan_gate_sibling_stays_10(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_auto_sibling_plan",
            repo_path: "/repo",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Auto),
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let row = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(row.plan_gate_timeout_min, 10);
    Ok(())
}

#[tokio::test]
async fn given_completion_policy_some_auto_when_created_then_soft_timeout_sibling_stays_60(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_auto_sibling_soft",
            repo_path: "/repo",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Auto),
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let row = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(row.completion_soft_timeout_min, 60);
    Ok(())
}

#[tokio::test]
async fn given_plan_gate_timeout_some_zero_when_created_then_plan_gate_is_0() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_plan_gate_zero",
            repo_path: "/repo",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: Some(0),
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let row = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(row.plan_gate_timeout_min, 0);
    Ok(())
}

#[tokio::test]
async fn given_plan_gate_timeout_some_zero_when_created_then_completion_policy_sibling_stays_manual(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_plan_gate_zero_sibling",
            repo_path: "/repo",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: Some(0),
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let row = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(row.completion_policy, CompletionPolicy::Manual);
    Ok(())
}

#[tokio::test]
async fn given_soft_timeout_30_and_policy_soft_when_created_then_soft_timeout_is_30(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_soft_30",
            repo_path: "/repo",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Soft),
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: Some(30),
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let row = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(row.completion_soft_timeout_min, 30);
    Ok(())
}

#[tokio::test]
async fn given_soft_timeout_30_and_policy_soft_when_created_then_completion_policy_is_soft(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_soft_30_policy",
            repo_path: "/repo",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Soft),
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: Some(30),
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let row = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(row.completion_policy, CompletionPolicy::Soft);
    Ok(())
}

#[tokio::test]
async fn given_all_three_overrides_when_created_then_each_persists_independently(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_trio",
            repo_path: "/r",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: Some(CompletionPolicy::Soft),
            plan_gate_timeout_min: Some(0),
            completion_soft_timeout_min: Some(45),
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(
        (
            p.completion_policy,
            p.plan_gate_timeout_min,
            p.completion_soft_timeout_min
        ),
        (CompletionPolicy::Soft, 0, 45)
    );
    Ok(())
}

#[tokio::test]
async fn given_negative_plan_gate_timeout_override_when_created_then_stored_verbatim(
) -> anyhow::Result<()> {
    // Structurally valid but semantically odd: no CHECK on the timeout columns,
    // so the override path stores a negative verbatim (and leaves siblings alone).
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_neg",
            repo_path: "/r",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: Some(-5),
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(
        (p.plan_gate_timeout_min, p.completion_soft_timeout_min),
        (-5, 60)
    );
    Ok(())
}

#[tokio::test]
async fn given_soft_timeout_override_alone_when_created_then_persists_and_policy_stays_manual(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = projects::create(
        db.pool(),
        NewProject {
            name: "proj_soft_alone",
            repo_path: "/r",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            route_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: Some(45),
            schedule_interval_min: None,
            schedule_cron: None,
        },
        TS,
    )
    .await?;
    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(
        (
            p.completion_policy,
            p.plan_gate_timeout_min,
            p.completion_soft_timeout_min
        ),
        (CompletionPolicy::Manual, 10, 45)
    );
    Ok(())
}

// ===========================================================================
// arsenal
// ===========================================================================

#[tokio::test]
async fn given_open_memory_db_when_list_arsenal_presets_then_builtin_names_are_seeded(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let presets = arsenal::list(db.pool()).await?;
    let names: Vec<_> = presets.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["claude", "codex"]);
    Ok(())
}

#[tokio::test]
async fn given_missing_arsenal_name_when_get_by_name_then_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let preset = arsenal::get_by_name(db.pool(), "missing").await?;
    assert!(preset.is_none());
    Ok(())
}

#[tokio::test]
async fn given_new_arsenal_preset_when_upserted_then_get_by_name_returns_it() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let id = upsert_preset(db.pool(), "local").await?;
    let preset = arsenal::get_by_name(db.pool(), "local")
        .await?
        .expect("preset exists");
    assert_eq!(
        (preset.id, preset.main_agent_cmd.as_str()),
        (id, "main {prompt}")
    );
    Ok(())
}

#[tokio::test]
async fn given_existing_custom_arsenal_preset_when_upserted_then_same_id_is_returned(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let first = upsert_preset(db.pool(), "local").await?;
    let second = arsenal::upsert(
        db.pool(),
        NewArsenalPreset {
            name: "local",
            main_agent_cmd: "main2",
            route_agent_cmd: "main2",
            plan_agent_cmd: "plan2",
            work_agent_cmd: "work2",
            review_agent_cmd: None,
        },
        TS2,
    )
    .await?;
    assert_eq!(second, first);
    Ok(())
}

#[tokio::test]
async fn given_existing_custom_arsenal_preset_when_upserted_then_command_is_updated(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    upsert_preset(db.pool(), "local").await?;
    arsenal::upsert(
        db.pool(),
        NewArsenalPreset {
            name: "local",
            main_agent_cmd: "main2",
            route_agent_cmd: "main2",
            plan_agent_cmd: "plan2",
            work_agent_cmd: "work2",
            review_agent_cmd: None,
        },
        TS2,
    )
    .await?;
    let preset = arsenal::get_by_name(db.pool(), "local")
        .await?
        .expect("preset exists");
    assert_eq!(preset.main_agent_cmd, "main2");
    Ok(())
}

#[tokio::test]
async fn given_blank_trimmed_arsenal_name_when_upserted_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let err = arsenal::upsert(
        db.pool(),
        NewArsenalPreset {
            name: "  ",
            main_agent_cmd: "main",
            route_agent_cmd: "main",
            plan_agent_cmd: "plan",
            work_agent_cmd: "work",
            review_agent_cmd: None,
        },
        TS,
    )
    .await
    .expect_err("blank name must be rejected");
    assert!(err.to_string().contains("name is required"));
    Ok(())
}

#[tokio::test]
async fn given_blank_trimmed_main_command_when_upserted_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let err = arsenal::upsert(
        db.pool(),
        NewArsenalPreset {
            name: "local",
            main_agent_cmd: " ",
            route_agent_cmd: " ",
            plan_agent_cmd: "plan",
            work_agent_cmd: "work",
            review_agent_cmd: None,
        },
        TS,
    )
    .await
    .expect_err("blank main command must be rejected");
    assert!(err.to_string().contains("commands are required"));
    Ok(())
}

#[tokio::test]
async fn given_blank_trimmed_plan_command_when_upserted_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let err = arsenal::upsert(
        db.pool(),
        NewArsenalPreset {
            name: "local",
            main_agent_cmd: "main",
            route_agent_cmd: "main",
            plan_agent_cmd: " ",
            work_agent_cmd: "work",
            review_agent_cmd: None,
        },
        TS,
    )
    .await
    .expect_err("blank plan command must be rejected");
    assert!(err.to_string().contains("commands are required"));
    Ok(())
}

#[tokio::test]
async fn given_blank_trimmed_work_command_when_upserted_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let err = arsenal::upsert(
        db.pool(),
        NewArsenalPreset {
            name: "local",
            main_agent_cmd: "main",
            route_agent_cmd: "main",
            plan_agent_cmd: "plan",
            work_agent_cmd: " ",
            review_agent_cmd: None,
        },
        TS,
    )
    .await
    .expect_err("blank work command must be rejected");
    assert!(err.to_string().contains("commands are required"));
    Ok(())
}

#[tokio::test]
async fn given_builtin_arsenal_name_when_upserted_then_builtin_becomes_false() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    arsenal::upsert(
        db.pool(),
        NewArsenalPreset {
            name: "codex",
            main_agent_cmd: "custom-main",
            route_agent_cmd: "custom-main",
            plan_agent_cmd: "custom-plan",
            work_agent_cmd: "custom-work",
            review_agent_cmd: None,
        },
        TS2,
    )
    .await?;
    let preset = arsenal::get_by_name(db.pool(), "codex")
        .await?
        .expect("preset exists");
    assert!(!preset.builtin);
    Ok(())
}

#[tokio::test]
async fn given_builtin_arsenal_name_when_upserted_then_new_command_persists() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    arsenal::upsert(
        db.pool(),
        NewArsenalPreset {
            name: "codex",
            main_agent_cmd: "custom-main",
            route_agent_cmd: "custom-main",
            plan_agent_cmd: "custom-plan",
            work_agent_cmd: "custom-work",
            review_agent_cmd: None,
        },
        TS2,
    )
    .await?;
    let preset = arsenal::get_by_name(db.pool(), "codex")
        .await?
        .expect("preset exists");
    assert_eq!(preset.main_agent_cmd, "custom-main");
    Ok(())
}

// ===========================================================================
// main jobs
// ===========================================================================

#[tokio::test]
async fn given_routine_main_job_when_enqueued_then_recent_by_project_returns_queued(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "main-job-project").await?;
    let job_id = insert_main_job(db.pool(), project_id).await?;

    let jobs = main_jobs::recent_by_project(db.pool(), project_id, 10).await?;

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, job_id);
    assert_eq!(jobs[0].status, MainJobStatus::Queued);
    assert_eq!(jobs[0].queued_at, TS);
    Ok(())
}

#[tokio::test]
async fn given_ask_answers_when_list_by_project_then_newest_first() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "ask-project").await?;
    let older = ask_answers::create(
        db.pool(),
        ask_answers::NewAskAnswer {
            project_id,
            mode: AskMode::Recall,
            question: "what is next?",
            answer: "older answer",
            context_summary: Some("ctx"),
            log_path: Some("target/auwsx-test/ask.log"),
        },
        TS,
    )
    .await?;
    let newer = ask_answers::create(
        db.pool(),
        ask_answers::NewAskAnswer {
            project_id,
            mode: AskMode::Seek,
            question: "what is blocked?",
            answer: "newer answer",
            context_summary: None,
            log_path: None,
        },
        TS2,
    )
    .await?;

    let answers = ask_answers::list_by_project(db.pool(), project_id, 10).await?;

    assert_eq!(
        answers.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![newer, older]
    );
    assert_eq!(answers[0].mode, AskMode::Seek);
    assert_eq!(answers[0].answer, "newer answer");
    Ok(())
}

#[tokio::test]
async fn given_ask_mode_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for v in [AskMode::Recall, AskMode::Seek] {
        assert_eq!(AskMode::parse(v.as_str()), Some(v), "{v:?}");
    }
    Ok(())
}

#[tokio::test]
async fn given_ask_mode_as_str_when_checked_then_matches_spec_ids() -> anyhow::Result<()> {
    assert_eq!(AskMode::Recall.as_str(), "recall");
    assert_eq!(AskMode::Seek.as_str(), "seek");
    Ok(())
}

#[tokio::test]
async fn given_bogus_or_empty_when_ask_mode_parse_then_none() -> anyhow::Result<()> {
    assert_eq!(AskMode::parse("bogus"), None);
    assert_eq!(AskMode::parse(""), None);
    Ok(())
}

#[tokio::test]
async fn given_queued_main_job_when_mark_running_then_status_and_log_are_set() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "main-job-running").await?;
    let job_id = insert_main_job(db.pool(), project_id).await?;

    main_jobs::mark_running(db.pool(), job_id, TS2, "/tmp/job.log").await?;
    let job = main_jobs::get(db.pool(), job_id)
        .await?
        .expect("job exists");

    assert_eq!(job.status, MainJobStatus::Running);
    assert_eq!(job.started_at, Some(TS2));
    assert_eq!(job.log_path.as_deref(), Some("/tmp/job.log"));
    Ok(())
}

#[tokio::test]
async fn given_done_main_job_when_mark_running_again_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "main-job-guard").await?;
    let job_id = insert_main_job(db.pool(), project_id).await?;
    main_jobs::finish(db.pool(), job_id, MainJobStatus::Done, TS2, Some("ok")).await?;

    let err = main_jobs::mark_running(db.pool(), job_id, TS2, "/tmp/job.log")
        .await
        .expect_err("terminal job must not be marked running");

    assert!(
        err.to_string().contains("is not queued"),
        "unexpected error: {err:#}"
    );
    Ok(())
}

#[tokio::test]
async fn given_non_terminal_status_when_finish_main_job_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "main-job-finish").await?;
    let job_id = insert_main_job(db.pool(), project_id).await?;

    let err = main_jobs::finish(db.pool(), job_id, MainJobStatus::Running, TS2, None)
        .await
        .expect_err("finish must reject non-terminal statuses");

    assert!(
        err.to_string().contains("terminal"),
        "unexpected error: {err:#}"
    );
    Ok(())
}

#[tokio::test]
async fn given_seeded_db_when_get_global_settings_then_returns_singleton() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;

    let settings = global_settings::get(db.pool()).await?;

    assert!(
        settings.pipeline_ux_guidance.contains("operator console"),
        "seeded guidance should preserve the auwsx UI standard"
    );
    assert!(
        settings
            .pipeline_ux_guidance
            .contains("avoid duplicate paths"),
        "seeded guidance should guard against duplicated interaction paths"
    );
    assert_eq!(settings.updated_at, 0);
    Ok(())
}

#[tokio::test]
async fn given_global_guidance_with_outer_whitespace_when_updated_then_stores_trimmed(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;

    global_settings::update_pipeline_ux_guidance(db.pool(), "  durable guidance\n", TS).await?;
    let settings = global_settings::get(db.pool()).await?;

    assert_eq!(settings.pipeline_ux_guidance, "durable guidance");
    assert_eq!(settings.updated_at, TS);
    Ok(())
}

#[tokio::test]
async fn given_global_guidance_over_limit_when_updated_then_err_and_existing_value_survives(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    global_settings::update_pipeline_ux_guidance(db.pool(), "keep", TS).await?;
    let too_long = "x".repeat(PIPELINE_UX_GUIDANCE_MAX_CHARS + 1);

    let err = global_settings::update_pipeline_ux_guidance(db.pool(), &too_long, TS2)
        .await
        .expect_err("overlong guidance must be rejected before persistence");
    let settings = global_settings::get(db.pool()).await?;

    assert!(
        err.to_string().contains("at most"),
        "unexpected error: {err:#}"
    );
    assert_eq!(settings.pipeline_ux_guidance, "keep");
    assert_eq!(settings.updated_at, TS);
    Ok(())
}

#[tokio::test]
async fn given_missing_global_settings_singleton_when_updated_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    sqlx::query("DELETE FROM global_settings WHERE id = 1")
        .execute(db.pool())
        .await?;

    let err = global_settings::update_pipeline_ux_guidance(db.pool(), "x", TS)
        .await
        .expect_err("missing singleton must not look like a successful update");

    assert!(
        err.to_string().contains("singleton missing"),
        "unexpected error: {err:#}"
    );
    Ok(())
}

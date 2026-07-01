//! Integration smoke tests for the auwsx-core SQLite schema.
//!
//! Source of truth: crates/auwsx-core/src/db/migrations/0001_init.sql
//! Public API exercised: `Db::open_memory()` + `Db::pool()`.
//!
//! These tests assert the PUBLIC CONTRACT only (table shape, NOT NULL columns,
//! CHECK domains, FK cascade). Failure cases are tested aggressively: every
//! CHECK-domain test supplies all other required columns correctly so the
//! single bad value is the only possible reason for rejection.

use auwsx_core::db::remote::{
    self, RecordRemoteEvent, RemoteAuthKind, RemotePrCheckStatus, RemotePrState, RemoteProvider,
    RequiredChecksPolicy, UpsertProjectRemoteConfig, UpsertRemoteIssueLink, UpsertRemotePrLink,
};
use auwsx_core::db::Db;
use sqlx::{Row, SqlitePool};

/// Fixed deterministic timestamp (Unix epoch ms) for every row. No SystemTime.
const TS: i64 = 1_000_000;

fn valid_remote_config(project_id: i64) -> UpsertProjectRemoteConfig<'static> {
    UpsertProjectRemoteConfig {
        project_id,
        provider: RemoteProvider::Github,
        remote_url: "https://github.com/acme/repo",
        owner: "acme",
        repo: "repo",
        api_base_url: "https://api.github.com",
        auth_kind: RemoteAuthKind::TokenEnv,
        auth_ref: Some("GITHUB_TOKEN"),
        webhook_secret_ref: Some("WEBHOOK_SECRET"),
        inbound_auwsx_run_enabled: true,
        outbound_issue_create_enabled: true,
        remote_pr_merge_enabled: false,
        agent_comment_sync_enabled: true,
        subtask_comment_sync_enabled: false,
        finding_comment_sync_enabled: true,
        draft_pr_enabled: true,
        required_checks_policy: RequiredChecksPolicy::Observe,
        default_labels: Some("auwsx"),
        default_assignees: Some("maintainer"),
        pr_base_branch: Some("main"),
    }
}

// ---------------------------------------------------------------------------
// Helpers: each inserts a row supplying EVERY NOT NULL column with a valid
// value, then returns the new rowid. Tests use these so they can never
// accidentally omit a required column.
// ---------------------------------------------------------------------------

/// Insert a valid `projects` row. NOT NULL cols supplied:
/// name, repo_path, default_branch, main_agent_cmd, plan_agent_cmd,
/// work_agent_cmd, created_at (all others have DEFAULTs in the SQL).
async fn insert_project(pool: &SqlitePool, name: &str) -> anyhow::Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO projects
            (name, repo_path, default_branch,
             main_agent_cmd, plan_agent_cmd, work_agent_cmd, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(name)
    .bind("/repo/path")
    .bind("main")
    .bind("claude {prompt}")
    .bind("claude-plan {prompt}")
    .bind("claude-work {prompt}")
    .bind(TS)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

/// Insert a valid `issues` row. NOT NULL cols supplied:
/// project_id, title, status, created_at, updated_at
/// (review_round, conflict_attempts, has_pending_steering have DEFAULTs).
async fn insert_issue(pool: &SqlitePool, project_id: i64, status: &str) -> anyhow::Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO issues (project_id, title, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(project_id)
    .bind("a title")
    .bind(status)
    .bind(TS)
    .bind(TS)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

/// Insert a valid `routines` row. NOT NULL cols supplied:
/// project_id, name, origin, type, prompt, cron, created_at
/// (enabled has a DEFAULT).
async fn insert_routine(pool: &SqlitePool, project_id: i64, name: &str) -> anyhow::Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO routines
            (project_id, name, origin, type, prompt, cron, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(project_id)
    .bind(name)
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

/// Insert a valid `main_jobs` row. NOT NULL cols supplied:
/// project_id, source, kind, prompt, status, queued_at.
async fn insert_main_job(pool: &SqlitePool, project_id: i64) -> anyhow::Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO main_jobs
            (project_id, source, kind, prompt, status, queued_at)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(project_id)
    .bind("routine")
    .bind("report")
    .bind("a prompt")
    .bind("QUEUED")
    .bind(TS)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

// ---------------------------------------------------------------------------
// 1. open_memory + table existence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_runtime_tables_exist() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let expected = [
        "arsenal_agent_presets",
        "ask_answers",
        "global_settings",
        "memory_presets",
        "profiles",
        "projects",
        "routines",
        "issues",
        "subtasks",
        "findings",
        "steering",
        "backlog_items",
        "main_jobs",
        "agent_runs",
        "routing_runs",
        "scheduler_runs",
        "project_route_locks",
        "project_remote_configs",
        "remote_issue_links",
        "remote_pr_links",
        "remote_events",
        "remote_sync_runs",
    ];
    let actual_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS n FROM sqlite_master WHERE type='table' AND name NOT LIKE '_sqlx%'",
    )
    .fetch_one(db.pool())
    .await?
    .get("n");
    assert_eq!(
        actual_count,
        expected.len() as i64,
        "runtime table count should match the expected migrated schema"
    );
    for table in expected {
        let count: i64 =
            sqlx::query("SELECT COUNT(*) AS n FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table)
                .fetch_one(db.pool())
                .await?
                .get("n");
        assert_eq!(count, 1, "table `{table}` should exist exactly once");
    }
    Ok(())
}

#[tokio::test]
async fn issues_agent_session_column_absent() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let columns: Vec<String> = sqlx::query("PRAGMA table_info(issues)")
        .fetch_all(db.pool())
        .await?
        .into_iter()
        .map(|row| row.get("name"))
        .collect();

    assert!(
        !columns.iter().any(|name| name == "agent_session"),
        "agent_session is a removed tmux-era field"
    );
    Ok(())
}

#[tokio::test]
async fn global_settings_seed_row_exists() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM global_settings WHERE id = 1")
        .fetch_one(db.pool())
        .await?
        .get("n");

    assert_eq!(count, 1, "global settings should have one singleton row");
    Ok(())
}

// ---------------------------------------------------------------------------
// 2. projects round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn project_row_roundtrips() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(db.pool(), "alpha").await?;

    let name: String = sqlx::query("SELECT name FROM projects WHERE id = ?")
        .bind(id)
        .fetch_one(db.pool())
        .await?
        .get("name");

    assert_eq!(name, "alpha");
    Ok(())
}

#[tokio::test]
async fn given_existing_project_remote_config_when_upserted_then_one_row_remains(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "remote-upsert").await?;
    remote::upsert_config(db.pool(), valid_remote_config(project_id), TS).await?;
    let mut updated = valid_remote_config(project_id);
    updated.repo = "renamed";

    remote::upsert_config(db.pool(), updated, TS + 1).await?;

    let count: i64 =
        sqlx::query("SELECT COUNT(*) AS n FROM project_remote_configs WHERE project_id = ?")
            .bind(project_id)
            .fetch_one(db.pool())
            .await?
            .get("n");
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
async fn given_optional_blank_remote_config_fields_when_upserted_then_they_are_null(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "remote-trim").await?;
    let mut input = valid_remote_config(project_id);
    input.webhook_secret_ref = Some(" ");
    input.default_labels = Some("");
    input.default_assignees = Some("  ");
    input.pr_base_branch = Some("\n");

    remote::upsert_config(db.pool(), input, TS).await?;

    let got = remote::get_config(db.pool(), project_id)
        .await?
        .expect("config");
    assert_eq!(
        (
            got.webhook_secret_ref,
            got.default_labels,
            got.default_assignees,
            got.pr_base_branch
        ),
        (None, None, None, None)
    );
    Ok(())
}

#[tokio::test]
async fn given_blank_required_remote_url_when_upserted_then_error() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "remote-required").await?;
    let mut input = valid_remote_config(project_id);
    input.remote_url = " ";

    assert!(remote::upsert_config(db.pool(), input, TS).await.is_err());
    Ok(())
}

#[tokio::test]
async fn given_token_auth_without_auth_ref_when_upserted_then_error() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "remote-auth").await?;
    let mut input = valid_remote_config(project_id);
    input.auth_ref = Some(" ");

    assert!(remote::upsert_config(db.pool(), input, TS).await.is_err());
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. issues round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn issue_row_roundtrips_with_planning_status() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "beta").await?;
    let issue_id = insert_issue(db.pool(), project_id, "PLANNING").await?;

    let status: String = sqlx::query("SELECT status FROM issues WHERE id = ?")
        .bind(issue_id)
        .fetch_one(db.pool())
        .await?
        .get("status");

    assert_eq!(status, "PLANNING");
    Ok(())
}

#[tokio::test]
async fn given_duplicate_remote_delivery_when_recorded_then_second_insert_is_ignored(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let first = remote::record_event(
        db.pool(),
        RecordRemoteEvent {
            project_id: None,
            provider: RemoteProvider::Github,
            delivery_id: "delivery-dup",
            event_kind: "issue_comment",
            action: Some("created"),
            payload_hash: "hash",
        },
        TS,
    )
    .await?;
    let second = remote::record_event(
        db.pool(),
        RecordRemoteEvent {
            project_id: None,
            provider: RemoteProvider::Github,
            delivery_id: "delivery-dup",
            event_kind: "issue_comment",
            action: Some("created"),
            payload_hash: "hash",
        },
        TS + 1,
    )
    .await?;

    assert!(first.is_some() && second.is_none());
    Ok(())
}

#[tokio::test]
async fn given_remote_issue_link_without_local_target_when_upserted_then_error(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "remote-link-invalid").await?;

    assert!(remote::upsert_issue_link(
        db.pool(),
        UpsertRemoteIssueLink {
            project_id,
            issue_id: None,
            backlog_item_id: None,
            provider: RemoteProvider::Github,
            remote_owner: "acme",
            remote_repo: "repo",
            remote_issue_number: 1,
            remote_node_id: None,
            remote_url: "https://github.com/acme/repo/issues/1",
            last_synced_at: None,
        },
        TS,
    )
    .await
    .is_err());
    Ok(())
}

#[tokio::test]
async fn given_issue_remote_issue_link_when_fetched_by_issue_then_returns_link(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "remote-link").await?;
    let issue_id = insert_issue(db.pool(), project_id, "PLANNING").await?;
    remote::upsert_issue_link(
        db.pool(),
        UpsertRemoteIssueLink {
            project_id,
            issue_id: Some(issue_id),
            backlog_item_id: None,
            provider: RemoteProvider::Github,
            remote_owner: "acme",
            remote_repo: "repo",
            remote_issue_number: 2,
            remote_node_id: None,
            remote_url: "https://github.com/acme/repo/issues/2",
            last_synced_at: None,
        },
        TS,
    )
    .await?;

    let got = remote::issue_link_by_issue(db.pool(), issue_id)
        .await?
        .expect("remote issue link");
    assert_eq!(got.remote_issue_number, 2);
    Ok(())
}

#[tokio::test]
async fn given_existing_issue_pr_link_when_upserted_again_then_fetch_returns_updated_link(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "remote-pr").await?;
    let issue_id = insert_issue(db.pool(), project_id, "PLANNING").await?;
    for (number, check_status) in [
        (10, RemotePrCheckStatus::Success),
        (11, RemotePrCheckStatus::Failure),
    ] {
        remote::upsert_pr_link(
            db.pool(),
            UpsertRemotePrLink {
                project_id,
                issue_id,
                provider: RemoteProvider::Github,
                remote_owner: "acme",
                remote_repo: "repo",
                remote_pr_number: number,
                remote_node_id: None,
                remote_url: "https://github.com/acme/repo/pull/11",
                head_branch: "auwsx/issue-1",
                head_sha: None,
                base_branch: "main",
                base_sha: None,
                state: RemotePrState::Open,
                check_status,
                check_summary: Some("check summary"),
                merge_state_status: None,
                review_decision: None,
                last_synced_at: None,
            },
            TS + number,
        )
        .await?;
    }

    let got = remote::pr_link_by_issue(db.pool(), issue_id)
        .await?
        .expect("remote PR link");
    assert_eq!(got.remote_pr_number, 11);
    assert_eq!(got.check_status, RemotePrCheckStatus::Failure);
    assert_eq!(got.check_summary.as_deref(), Some("check summary"));
    Ok(())
}

// ---------------------------------------------------------------------------
// 4. FK cascade: deleting a project zeroes every child table referencing it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deleting_project_cascades_to_children() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let pool = db.pool();

    let project_id = insert_project(pool, "gamma").await?;
    insert_issue(pool, project_id, "PLANNING").await?;
    insert_routine(pool, project_id, "r1").await?;
    insert_main_job(pool, project_id).await?;
    sqlx::query(
        "INSERT INTO backlog_items (project_id, text, source, created_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind("an item")
    .bind("human")
    .bind(TS)
    .execute(pool)
    .await?;

    let deleted = sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(project_id)
        .execute(pool)
        .await?
        .rows_affected();
    // Prove the DELETE actually hit the parent; otherwise the child-count
    // assertions below could pass vacuously against an id that was never there.
    assert_eq!(deleted, 1, "exactly one project row should be deleted");

    let parent_remaining: i64 = sqlx::query("SELECT COUNT(*) AS n FROM projects WHERE id = ?")
        .bind(project_id)
        .fetch_one(pool)
        .await?
        .get("n");
    assert_eq!(parent_remaining, 0, "parent project row must be gone");

    for table in ["issues", "routines", "main_jobs", "backlog_items"] {
        let remaining: i64 = sqlx::query(&format!(
            "SELECT COUNT(*) AS n FROM {table} WHERE project_id = ?"
        ))
        .bind(project_id)
        .fetch_one(pool)
        .await?
        .get("n");
        assert_eq!(remaining, 0, "`{table}` rows should cascade-delete");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 5. CHECK-domain rejections. Every other required column is valid; the bad
//    value is the only possible cause of failure.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn issues_status_bad_value_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;

    let res = sqlx::query(
        "INSERT INTO issues (project_id, title, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind("t")
    .bind("BOGUS")
    .bind(TS)
    .bind(TS)
    .execute(db.pool())
    .await;

    assert!(res.is_err(), "issues.status='BOGUS' must be rejected");
    Ok(())
}

#[tokio::test]
async fn projects_completion_policy_bad_value_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;

    let res = sqlx::query(
        "INSERT INTO projects
            (name, repo_path, default_branch,
             main_agent_cmd, plan_agent_cmd, work_agent_cmd, created_at,
             completion_policy)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("p")
    .bind("/repo")
    .bind("main")
    .bind("c {prompt}")
    .bind("c {prompt}")
    .bind("c {prompt}")
    .bind(TS)
    .bind("whatever")
    .execute(db.pool())
    .await;

    assert!(
        res.is_err(),
        "projects.completion_policy='whatever' must be rejected"
    );
    Ok(())
}

#[tokio::test]
async fn backlog_items_approval_bad_value_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;

    let res = sqlx::query(
        "INSERT INTO backlog_items (project_id, text, source, approval, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind("an item")
    .bind("human")
    .bind("maybe")
    .bind(TS)
    .execute(db.pool())
    .await;

    assert!(
        res.is_err(),
        "backlog_items.approval='maybe' must be rejected"
    );
    Ok(())
}

#[tokio::test]
async fn routines_type_bad_value_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;

    let res = sqlx::query(
        "INSERT INTO routines
            (project_id, name, origin, type, prompt, cron, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind("r")
    .bind("user")
    .bind("bad")
    .bind("a prompt")
    .bind("0 0 * * * *")
    .bind(TS)
    .execute(db.pool())
    .await;

    assert!(res.is_err(), "routines.type='bad' must be rejected");
    Ok(())
}

#[tokio::test]
async fn main_jobs_status_bad_value_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;

    let res = sqlx::query(
        "INSERT INTO main_jobs
            (project_id, source, kind, prompt, status, queued_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind("routine")
    .bind("report")
    .bind("a prompt")
    .bind("RUNNINGX")
    .bind(TS)
    .execute(db.pool())
    .await;

    assert!(res.is_err(), "main_jobs.status='RUNNINGX' must be rejected");
    Ok(())
}

#[tokio::test]
async fn findings_severity_bad_value_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;
    let issue_id = insert_issue(db.pool(), project_id, "REVIEWING").await?;

    let res = sqlx::query(
        "INSERT INTO findings
            (issue_id, review_round, severity, title, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(issue_id)
    .bind(0)
    .bind("huge")
    .bind("a finding")
    .bind(TS)
    .execute(db.pool())
    .await;

    assert!(res.is_err(), "findings.severity='huge' must be rejected");
    Ok(())
}

// ---------------------------------------------------------------------------
// 5b. CHECK positive controls. Each rejection test above only asserts is_err();
//     a typo'd column, a missing NOT NULL, or an FK miss would *also* produce
//     is_err() and hand us a false green. These prove the SAME insert with a
//     KNOWN-VALID domain value succeeds, so the rejection above is attributable
//     to the bad value alone.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn issues_status_valid_value_accepted() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;
    // Same column set as issues_status_bad_value_rejected, valid status.
    sqlx::query(
        "INSERT INTO issues (project_id, title, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind("t")
    .bind("PLANNING")
    .bind(TS)
    .bind(TS)
    .execute(db.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn projects_completion_policy_valid_value_accepted() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    // Same column set as projects_completion_policy_bad_value_rejected.
    // 'auto' is a plausible valid domain member; if the real domain differs the
    // rejection test's contract is wrong, which this surfaces.
    sqlx::query(
        "INSERT INTO projects
            (name, repo_path, default_branch,
             main_agent_cmd, plan_agent_cmd, work_agent_cmd, created_at,
             completion_policy)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("p")
    .bind("/repo")
    .bind("main")
    .bind("c {prompt}")
    .bind("c {prompt}")
    .bind("c {prompt}")
    .bind(TS)
    .bind("auto")
    .execute(db.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn backlog_items_approval_valid_value_accepted() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;
    sqlx::query(
        "INSERT INTO backlog_items (project_id, text, source, approval, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind("an item")
    .bind("human")
    .bind("approved")
    .bind(TS)
    .execute(db.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn main_jobs_status_valid_value_accepted() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;
    sqlx::query(
        "INSERT INTO main_jobs
            (project_id, source, kind, prompt, status, queued_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind("routine")
    .bind("report")
    .bind("a prompt")
    .bind("RUNNING")
    .bind(TS)
    .execute(db.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn findings_severity_valid_value_accepted() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;
    let issue_id = insert_issue(db.pool(), project_id, "REVIEWING").await?;
    sqlx::query(
        "INSERT INTO findings
            (issue_id, review_round, severity, title, created_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(issue_id)
    .bind(0)
    .bind("major")
    .bind("a finding")
    .bind(TS)
    .execute(db.pool())
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 6. NOT NULL enforcement. The CHECK tests prove the domain is constrained;
//    these prove required columns are actually required. Each omits exactly one
//    NOT NULL column while supplying every other column with a VALID value (so
//    only the omission can cause the failure), and a positive control proves
//    the same insert succeeds once the column is present.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn issues_title_omitted_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let project_id = insert_project(db.pool(), "p").await?;
    // status is a valid domain value, FK is valid, timestamps present; only
    // title is missing.
    let res = sqlx::query(
        "INSERT INTO issues (project_id, status, created_at, updated_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(project_id)
    .bind("PLANNING")
    .bind(TS)
    .bind(TS)
    .execute(db.pool())
    .await;
    assert!(
        res.is_err(),
        "issues.title is NOT NULL; omission must be rejected"
    );

    // Positive control: identical row with title present succeeds.
    insert_issue(db.pool(), project_id, "PLANNING").await?;
    Ok(())
}

#[tokio::test]
async fn projects_repo_path_omitted_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    // Every column except repo_path supplied with a valid value.
    let res = sqlx::query(
        "INSERT INTO projects
            (name, default_branch,
             main_agent_cmd, plan_agent_cmd, work_agent_cmd, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind("p")
    .bind("main")
    .bind("c {prompt}")
    .bind("c {prompt}")
    .bind("c {prompt}")
    .bind(TS)
    .execute(db.pool())
    .await;
    assert!(
        res.is_err(),
        "projects.repo_path is NOT NULL; omission must be rejected"
    );

    // Positive control: full valid row succeeds.
    insert_project(db.pool(), "p").await?;
    Ok(())
}

#[tokio::test]
async fn issues_project_id_omitted_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    // No project_id; all other NOT NULL columns valid.
    let res = sqlx::query(
        "INSERT INTO issues (title, status, created_at, updated_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind("t")
    .bind("PLANNING")
    .bind(TS)
    .bind(TS)
    .execute(db.pool())
    .await;
    assert!(
        res.is_err(),
        "issues.project_id is NOT NULL; omission must be rejected"
    );
    Ok(())
}

#[tokio::test]
async fn issues_bad_project_id_fk_rejected() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    // Valid shape, but project_id references a non-existent project. Proves FK
    // enforcement is actually ON (PRAGMA foreign_keys), which the cascade test
    // assumes but never isolates.
    let res = sqlx::query(
        "INSERT INTO issues (project_id, title, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(999_999_i64)
    .bind("t")
    .bind("PLANNING")
    .bind(TS)
    .bind(TS)
    .execute(db.pool())
    .await;
    assert!(
        res.is_err(),
        "issues.project_id FK to missing project must be rejected"
    );
    Ok(())
}

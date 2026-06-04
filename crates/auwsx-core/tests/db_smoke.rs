//! Public-contract smoke tests for `auwsx_core::db::Db`.
//!
//! Each test opens a fresh DB (in-memory or fresh tempdir on-disk) and
//! exercises one observable behaviour through the public API only:
//! migrations run on open, basic round-trip, UNIQUE/FK enforcement,
//! WAL on disk, reopen idempotence, default values, compound UNIQUE,
//! ON DELETE SET NULL on main_jobs.routine_id, and pool lifecycle.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use auwsx_core::db::Db;
use sqlx::Row;

const EXPECTED_TABLES: &[&str] = &[
    "projects",
    "tasks",
    "iterations",
    "feedback",
    "scheduler_runs",
    "drafts",
    "followups",
    "routines",
    "main_jobs",
];

const TS: i64 = 1_700_000_000_000;

async fn table_set(db: &Db) -> Result<BTreeSet<String>> {
    let rows = sqlx::query(
        "SELECT name FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE '_sqlx_%' ORDER BY name",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("name")).collect())
}

/// Insert a minimal project (all NOT NULL columns supplied; defaultable ones
/// left unset so per-test assertions on defaults remain meaningful).
async fn insert_project(db: &Db, name: &str) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO projects (name, repo_path, default_branch, agent, created_at) \
         VALUES (?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind("/repos/x")
    .bind("main")
    .bind("claude")
    .bind(TS)
    .fetch_one(db.pool())
    .await?;
    Ok(id)
}

/// Insert a minimal task (all NOT NULL columns supplied).
async fn insert_task(db: &Db, project_id: i64, title: &str) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO tasks (project_id, title, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(project_id)
    .bind(title)
    .bind("BACKLOG")
    .bind(TS)
    .bind(TS)
    .fetch_one(db.pool())
    .await?;
    Ok(id)
}

/// Insert a minimal routine (all NOT NULL columns supplied).
async fn insert_routine(db: &Db, project_id: i64, name: &str) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO routines (project_id, name, origin, prompt_template, cron, created_at) \
         VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(project_id)
    .bind(name)
    .bind("user")
    .bind("Run /something")
    .bind("0 7 * * *")
    .bind(TS)
    .fetch_one(db.pool())
    .await?;
    Ok(id)
}

/// Insert a minimal main_job (all NOT NULL columns supplied).
async fn insert_main_job(db: &Db, project_id: i64, routine_id: Option<i64>) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO main_jobs (project_id, routine_id, source, kind, prompt, status) \
         VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(project_id)
    .bind(routine_id)
    .bind("routine")
    .bind("custom")
    .bind("dummy prompt")
    .bind("QUEUED")
    .fetch_one(db.pool())
    .await?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Migrations + schema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_memory_runs_migrations() -> Result<()> {
    let db = Db::open_memory().await?;
    let actual = table_set(&db).await?;
    let expected: BTreeSet<String> = EXPECTED_TABLES.iter().map(|s| s.to_string()).collect();
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn open_at_creates_file_and_parent() -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let path = tmp.path().join("sub").join("dir").join("state.db");
    let db = Db::open_at(&path).await?;
    assert!(path.exists(), "db file not created at {path:?}");
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='projects'",
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
async fn reopen_on_disk_db_preserves_rows_and_schema() -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let path = tmp.path().join("state.db");

    let db1 = Db::open_at(&path).await?;
    let schema1 = table_set(&db1).await?;
    let id = insert_project(&db1, "alpha").await?;
    db1.close().await;

    let db2 = Db::open_at(&path).await?;
    let schema2 = table_set(&db2).await?;
    assert_eq!(schema1, schema2, "schema diverged on reopen");

    let name: String = sqlx::query_scalar("SELECT name FROM projects WHERE id = ?")
        .bind(id)
        .fetch_one(db2.pool())
        .await?;
    assert_eq!(name, "alpha");
    Ok(())
}

#[tokio::test]
async fn open_at_is_idempotent_when_target_exists_empty_file() -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let path = tmp.path().join("preexisting.db");
    std::fs::write(&path, b"")?;
    let db = Db::open_at(&path).await?;
    assert_eq!(table_set(&db).await?.len(), EXPECTED_TABLES.len());
    Ok(())
}

#[tokio::test]
async fn wal_journal_mode_on_disk_only() -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let path = tmp.path().join("wal.db");
    let disk = Db::open_at(&path).await?;
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(disk.pool())
        .await?;
    assert_eq!(mode.to_lowercase(), "wal", "on-disk DB must report WAL");

    let mem = Db::open_memory().await?;
    let mem_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(mem.pool())
        .await?;
    assert_ne!(
        mem_mode.to_lowercase(),
        "wal",
        "in-memory DB must not report WAL"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// projects: round-trip, defaults, uniqueness
// ---------------------------------------------------------------------------

#[tokio::test]
async fn insert_and_read_project_row() -> Result<()> {
    let db = Db::open_memory().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO projects (\
            name, repo_path, default_branch, agent, \
            schedule_interval_min, max_concurrency, merge_mode, \
            deepsleep_interval_days, last_deepsleep_at, created_at\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind("proj-a")
    .bind("/repos/alpha")
    .bind("main")
    .bind("claude")
    .bind(30_i64)
    .bind(4_i64)
    .bind("manual")
    .bind(14_i64)
    .bind(TS)
    .bind(TS + 1)
    .fetch_one(db.pool())
    .await?;

    let row = sqlx::query(
        "SELECT name, repo_path, default_branch, agent, \
                schedule_interval_min, max_concurrency, merge_mode, \
                deepsleep_interval_days, last_deepsleep_at, created_at \
         FROM projects WHERE id = ?",
    )
    .bind(id)
    .fetch_one(db.pool())
    .await?;

    assert_eq!(row.get::<String, _>("name"), "proj-a");
    assert_eq!(row.get::<String, _>("repo_path"), "/repos/alpha");
    assert_eq!(row.get::<String, _>("default_branch"), "main");
    assert_eq!(row.get::<String, _>("agent"), "claude");
    assert_eq!(row.get::<i64, _>("schedule_interval_min"), 30);
    assert_eq!(row.get::<i64, _>("max_concurrency"), 4);
    assert_eq!(row.get::<String, _>("merge_mode"), "manual");
    assert_eq!(row.get::<i64, _>("deepsleep_interval_days"), 14);
    assert_eq!(row.get::<i64, _>("last_deepsleep_at"), TS);
    assert_eq!(row.get::<i64, _>("created_at"), TS + 1);
    Ok(())
}

#[tokio::test]
async fn project_defaults_applied_when_columns_omitted() -> Result<()> {
    let db = Db::open_memory().await?;
    let id = insert_project(&db, "defaults-test").await?;
    let row = sqlx::query(
        "SELECT max_concurrency, merge_mode, deepsleep_interval_days \
         FROM projects WHERE id = ?",
    )
    .bind(id)
    .fetch_one(db.pool())
    .await?;
    assert_eq!(row.get::<i64, _>("max_concurrency"), 1);
    assert_eq!(row.get::<String, _>("merge_mode"), "auto");
    assert_eq!(row.get::<i64, _>("deepsleep_interval_days"), 7);
    Ok(())
}

#[tokio::test]
async fn unique_project_name_violation() -> Result<()> {
    let db = Db::open_memory().await?;
    insert_project(&db, "dup").await?;
    let err = insert_project(&db, "dup").await.err();
    assert!(err.is_some(), "duplicate project name must violate UNIQUE");
    Ok(())
}

// ---------------------------------------------------------------------------
// Foreign keys: cascade + set-null + violation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn foreign_key_cascade_on_project_delete() -> Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(&db, "p-cascade").await?;
    insert_task(&db, pid, "t1").await?;

    sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(pid)
        .execute(db.pool())
        .await?;

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE project_id = ?")
        .bind(pid)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(remaining, 0, "tasks must cascade-delete with project");
    Ok(())
}

#[tokio::test]
async fn delete_project_cascades_main_jobs() -> Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(&db, "p-mj").await?;
    insert_main_job(&db, pid, None).await?;

    sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(pid)
        .execute(db.pool())
        .await?;

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM main_jobs WHERE project_id = ?")
        .bind(pid)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(n, 0, "main_jobs must cascade-delete with project");
    Ok(())
}

#[tokio::test]
async fn delete_routine_sets_main_job_routine_id_null() -> Result<()> {
    let db = Db::open_memory().await?;
    let pid = insert_project(&db, "p-routine").await?;
    let rid = insert_routine(&db, pid, "nightly").await?;
    let mjid = insert_main_job(&db, pid, Some(rid)).await?;

    sqlx::query("DELETE FROM routines WHERE id = ?")
        .bind(rid)
        .execute(db.pool())
        .await?;

    let still_there: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM main_jobs WHERE id = ?")
            .bind(mjid)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(still_there, 1, "main_job must survive routine deletion");

    let routine_id: Option<i64> =
        sqlx::query_scalar("SELECT routine_id FROM main_jobs WHERE id = ?")
            .bind(mjid)
            .fetch_one(db.pool())
            .await?;
    assert!(
        routine_id.is_none(),
        "routine_id must be NULL after ON DELETE SET NULL"
    );
    Ok(())
}

#[tokio::test]
async fn foreign_key_violation_on_orphan_task() -> Result<()> {
    let db = Db::open_memory().await?;
    let err = sqlx::query(
        "INSERT INTO tasks (project_id, title, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(99_999_i64)
    .bind("orphan")
    .bind("BACKLOG")
    .bind(TS)
    .bind(TS)
    .execute(db.pool())
    .await
    .err();
    assert!(err.is_some(), "orphan FK must be rejected (PRAGMA fk=ON)");
    Ok(())
}

// ---------------------------------------------------------------------------
// routines: compound UNIQUE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn routine_unique_is_per_project_not_global() -> Result<()> {
    let db = Db::open_memory().await?;
    let p1 = insert_project(&db, "p1").await?;
    let p2 = insert_project(&db, "p2").await?;

    insert_routine(&db, p1, "nightly").await?;
    insert_routine(&db, p2, "nightly").await?;
    let dup = insert_routine(&db, p1, "nightly").await.err();
    assert!(dup.is_some(), "duplicate (project_id, name) must fail");
    Ok(())
}

// ---------------------------------------------------------------------------
// Semantically wrong but structurally valid
// ---------------------------------------------------------------------------

#[tokio::test]
async fn insert_task_with_invalid_status_semantically_wrong() -> Result<()> {
    // Schema only declares status NOT NULL — no CHECK constraint today.
    // This test pins current behaviour: the row is accepted. A future
    // migration adding a CHECK constraint will trip this and force an
    // explicit contract decision.
    let db = Db::open_memory().await?;
    let pid = insert_project(&db, "p-semantic").await?;
    let res = sqlx::query(
        "INSERT INTO tasks (project_id, title, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(pid)
    .bind("t-semantic")
    .bind("not-a-real-status-\u{1F600}")
    .bind(TS)
    .bind(TS)
    .execute(db.pool())
    .await;
    assert!(
        res.is_ok(),
        "no CHECK on tasks.status today — pin this; revisit when a constraint is added"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Pool lifecycle + concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_queries_share_pool() -> Result<()> {
    let db = Arc::new(Db::open_memory().await?);
    let a = {
        let db = Arc::clone(&db);
        tokio::spawn(async move {
            sqlx::query_scalar::<_, i64>("SELECT 1")
                .fetch_one(db.pool())
                .await
        })
    };
    let b = {
        let db = Arc::clone(&db);
        tokio::spawn(async move {
            sqlx::query_scalar::<_, i64>("SELECT 2")
                .fetch_one(db.pool())
                .await
        })
    };
    let (ra, rb) = tokio::join!(a, b);
    assert_eq!(ra??, 1);
    assert_eq!(rb??, 2);
    Ok(())
}

#[tokio::test]
async fn close_blocks_subsequent_queries() -> Result<()> {
    let db = Db::open_memory().await?;
    let pool = db.pool().clone();
    db.close().await;
    let res = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&pool)
        .await;
    assert!(res.is_err(), "queries after close() must fail");
    Ok(())
}

//! Integration tests for the agent subprocess runner and its DB action log.
//!
//! Source of truth: crates/auwsx-core/src/agent/mod.rs (build_argv, run,
//! ExitKind, AgentSpec, AgentOutcome) + crates/auwsx-core/src/db/agent_runs.rs
//! (Role, StartRun, start/finish/get/list_by_issue).
//!
//! Asserts the PUBLIC CONTRACT only. No mocking: `run` is exercised against real
//! `/bin/sh`, `echo`, `cat`, `sleep`, `pwd` and small 0o755 scripts created in a
//! `tempfile::tempdir()`. Log files are read back with `std::fs::read_to_string`.
//! Failure cases (bad binary, timeout, missing run id, xor guard) are tested
//! aggressively. Each `run` test uses a generous timeout except the timeout test.

use auwsx_core::agent::{self, AgentSpec, ExitKind};
use auwsx_core::db::agent_runs::{self, Role, StartRun};
use auwsx_core::db::issues;
use auwsx_core::db::projects::{self, NewProject};
use auwsx_core::db::Db;
use sqlx::SqlitePool;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

/// Fixed deterministic timestamps (Unix epoch ms). No SystemTime.
const TS: i64 = 1_000_000;
const TS2: i64 = 2_000_000;

/// A generous deadline so non-timeout `run` tests never race the killer.
const GENEROUS: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Insert a valid project via the public CRUD, returning its id.
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

/// Create a real issue under a fresh project; returns `(project_id, issue_id)`.
async fn insert_issue(pool: &SqlitePool, name: &str) -> anyhow::Result<(i64, i64)> {
    let pid = insert_project(pool, name).await?;
    let iid = issues::create(pool, pid, "a title", None, TS).await?;
    Ok((pid, iid))
}

/// Write an executable shell script (mode 0o755) at `path`.
fn write_script(path: &Path, body: &str) -> anyhow::Result<()> {
    std::fs::write(path, body)?;
    let mut perm = std::fs::metadata(path)?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm)?;
    Ok(())
}

/// A minimal `StartRun` bound to an issue, parameterized only by what the tests
/// actually assert. Keeps every required field populated with a valid value.
fn start_for_issue(issue_id: i64) -> StartRun<'static> {
    StartRun {
        issue_id: Some(issue_id),
        main_job_id: None,
        role: Role::Work,
        phase: "implement",
        agent_cmd: "claude {prompt}",
        status_before: Some("WORKING"),
        pid: Some(1234),
        prompt_path: Some("/tmp/prompt.txt"),
        log_path: Some("/tmp/run.log"),
    }
}

// ===========================================================================
// Contract A: build_argv (pure)
// ===========================================================================

#[test]
fn given_template_with_prompt_token_and_spaced_prompt_when_build_argv_then_prompt_is_one_arg(
) -> anyhow::Result<()> {
    let (argv, stdin) = agent::build_argv("claude --print {prompt}", "hello world")?;
    assert_eq!(
        (argv, stdin),
        (
            vec!["claude".into(), "--print".into(), "hello world".into()],
            false
        )
    );
    Ok(())
}

#[test]
fn given_template_that_is_only_prompt_token_when_build_argv_then_single_arg_no_stdin(
) -> anyhow::Result<()> {
    let (argv, stdin) = agent::build_argv("{prompt}", "x")?;
    assert_eq!((argv, stdin), (vec!["x".to_string()], false));
    Ok(())
}

#[test]
fn given_prompt_token_inside_a_token_when_build_argv_then_substring_replaced_in_place(
) -> anyhow::Result<()> {
    let (argv, _) = agent::build_argv("--flag=pre{prompt}post", "MID")?;
    assert_eq!(argv, vec!["--flag=preMIDpost".to_string()]);
    Ok(())
}

#[test]
fn given_template_with_no_prompt_token_when_build_argv_then_stdin_flag_true() -> anyhow::Result<()>
{
    let (argv, stdin) = agent::build_argv("opencode run -q", "anything")?;
    assert_eq!(
        (argv, stdin),
        (vec!["opencode".into(), "run".into(), "-q".into()], true)
    );
    Ok(())
}

#[test]
fn given_empty_template_when_build_argv_then_err() -> anyhow::Result<()> {
    assert!(
        agent::build_argv("", "x").is_err(),
        "empty template must Err"
    );
    Ok(())
}

#[test]
fn given_whitespace_only_template_when_build_argv_then_err() -> anyhow::Result<()> {
    assert!(
        agent::build_argv("   ", "x").is_err(),
        "whitespace-only template must Err"
    );
    Ok(())
}

// ===========================================================================
// Contract A: run (real subprocesses)
// ===========================================================================

#[tokio::test]
async fn given_echo_when_run_then_exit_kind_exited() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    let out = agent::run(AgentSpec {
        cmd_template: "echo {prompt}",
        prompt: "captured-stdout",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert_eq!(out.exit_kind, ExitKind::Exited);
    Ok(())
}

#[tokio::test]
async fn given_echo_when_run_then_exit_code_zero() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    let out = agent::run(AgentSpec {
        cmd_template: "echo {prompt}",
        prompt: "captured-stdout",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert_eq!(out.exit_code, Some(0));
    Ok(())
}

#[tokio::test]
async fn given_echo_when_run_then_log_contains_prompt() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    agent::run(AgentSpec {
        cmd_template: "echo {prompt}",
        prompt: "captured-stdout",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert!(std::fs::read_to_string(&log)?.contains("captured-stdout"));
    Ok(())
}

#[tokio::test]
async fn given_script_that_exits_3_when_run_then_exit_kind_exited() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let script = tmp.path().join("exit3.sh");
    write_script(&script, "#!/bin/sh\nexit 3\n")?;
    let log = tmp.path().join("run.log");
    let out = agent::run(AgentSpec {
        cmd_template: script.to_str().unwrap(),
        prompt: "",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert_eq!(out.exit_kind, ExitKind::Exited);
    Ok(())
}

#[tokio::test]
async fn given_script_that_exits_3_when_run_then_exit_code_three() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let script = tmp.path().join("exit3.sh");
    write_script(&script, "#!/bin/sh\nexit 3\n")?;
    let log = tmp.path().join("run.log");
    let out = agent::run(AgentSpec {
        cmd_template: script.to_str().unwrap(),
        prompt: "",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert_eq!(out.exit_code, Some(3));
    Ok(())
}

#[tokio::test]
async fn given_cat_with_no_prompt_token_when_run_then_prompt_piped_to_stdin_appears_in_log(
) -> anyhow::Result<()> {
    // `cat` has no {prompt} token -> prompt goes to stdin; cat echoes it back.
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    agent::run(AgentSpec {
        cmd_template: "cat",
        prompt: "from-stdin-only",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert!(std::fs::read_to_string(&log)?.contains("from-stdin-only"));
    Ok(())
}

#[tokio::test]
async fn given_cat_with_no_prompt_token_when_run_then_exit_code_zero() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    let out = agent::run(AgentSpec {
        cmd_template: "cat",
        prompt: "from-stdin-only",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert_eq!(out.exit_code, Some(0));
    Ok(())
}

#[tokio::test]
async fn given_sleep_exceeding_deadline_when_run_then_exit_kind_timeout() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    let out = agent::run(AgentSpec {
        cmd_template: "sleep 5",
        prompt: "",
        cwd: tmp.path(),
        log_path: &log,
        timeout: Duration::from_millis(200),
        env: &[],
    })
    .await?;
    assert_eq!(out.exit_kind, ExitKind::Timeout);
    Ok(())
}

#[tokio::test]
async fn given_sleep_exceeding_deadline_when_run_then_exit_code_none() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    let out = agent::run(AgentSpec {
        cmd_template: "sleep 5",
        prompt: "",
        cwd: tmp.path(),
        log_path: &log,
        timeout: Duration::from_millis(200),
        env: &[],
    })
    .await?;
    assert_eq!(out.exit_code, None);
    Ok(())
}

#[tokio::test]
async fn given_bogus_binary_when_run_then_exit_kind_error() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    let out = agent::run(AgentSpec {
        cmd_template: "this-binary-does-not-exist-auwsx {prompt}",
        prompt: "p",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert_eq!(out.exit_kind, ExitKind::Error);
    Ok(())
}

#[tokio::test]
async fn given_bogus_binary_when_run_then_pid_none() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    let out = agent::run(AgentSpec {
        cmd_template: "this-binary-does-not-exist-auwsx {prompt}",
        prompt: "p",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert_eq!(out.pid, None);
    Ok(())
}

#[tokio::test]
async fn given_bogus_binary_when_run_then_returns_ok_not_err() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    let res = agent::run(AgentSpec {
        cmd_template: "this-binary-does-not-exist-auwsx {prompt}",
        prompt: "p",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await;
    assert!(
        res.is_ok(),
        "spawn failure must be an Ok(Error) outcome, not Err"
    );
    Ok(())
}

#[tokio::test]
async fn given_bogus_binary_when_run_then_log_file_is_non_empty_failure_note() -> anyhow::Result<()>
{
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    agent::run(AgentSpec {
        cmd_template: "this-binary-does-not-exist-auwsx {prompt}",
        prompt: "p",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    assert!(
        !std::fs::read_to_string(&log)?.is_empty(),
        "failure note must be written to log"
    );
    Ok(())
}

#[tokio::test]
async fn given_extra_env_when_run_then_child_sees_it_in_log() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let script = tmp.path().join("echo_env.sh");
    write_script(&script, "#!/bin/sh\necho \"$AUWSX_ISSUE_ID\"\n")?;
    let log = tmp.path().join("run.log");
    agent::run(AgentSpec {
        cmd_template: script.to_str().unwrap(),
        prompt: "",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[("AUWSX_ISSUE_ID".to_string(), "42".to_string())],
    })
    .await?;
    assert!(std::fs::read_to_string(&log)?.contains("42"));
    Ok(())
}

#[tokio::test]
async fn given_cwd_set_when_run_pwd_then_log_reports_that_dir() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let log = tmp.path().join("run.log");
    agent::run(AgentSpec {
        cmd_template: "pwd",
        prompt: "",
        cwd: tmp.path(),
        log_path: &log,
        timeout: GENEROUS,
        env: &[],
    })
    .await?;
    // macOS symlinks /var -> /private/var; canonicalize both sides before compare.
    let logged = std::fs::read_to_string(&log)?;
    let printed = Path::new(logged.trim()).canonicalize()?;
    let expected = tmp.path().canonicalize()?;
    assert_eq!(printed, expected);
    Ok(())
}

// ===========================================================================
// Contract A: ExitKind enum round-trip
// ===========================================================================

#[test]
fn given_exit_kind_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for k in [
        ExitKind::Exited,
        ExitKind::Timeout,
        ExitKind::Killed,
        ExitKind::Error,
    ] {
        assert_eq!(ExitKind::from_str(k.as_str()), Some(k), "{k:?}");
    }
    Ok(())
}

#[test]
fn given_bogus_when_exit_kind_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(ExitKind::from_str("nope"), None);
    Ok(())
}

// ===========================================================================
// Contract B: db::agent_runs
// ===========================================================================

#[tokio::test]
async fn given_issue_only_start_when_get_then_role_phase_cmd_set() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let id = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    let run = agent_runs::get(db.pool(), id).await?.expect("run exists");
    assert_eq!(
        (run.role, run.phase.as_str(), run.agent_cmd.as_str()),
        (Role::Work, "implement", "claude {prompt}")
    );
    Ok(())
}

#[tokio::test]
async fn given_issue_only_start_when_get_then_status_before_and_pid_and_log_path_set(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let id = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    let run = agent_runs::get(db.pool(), id).await?.expect("run exists");
    assert_eq!(
        (
            run.status_before.as_deref(),
            run.pid,
            run.log_path.as_deref()
        ),
        (Some("WORKING"), Some(1234), Some("/tmp/run.log"))
    );
    Ok(())
}

#[tokio::test]
async fn given_issue_only_start_when_get_then_exit_fields_unset() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let id = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    let run = agent_runs::get(db.pool(), id).await?.expect("run exists");
    assert_eq!(
        (run.exit_kind, run.exited_at, run.status_after),
        (None, None, None)
    );
    Ok(())
}

#[tokio::test]
async fn given_both_issue_and_main_job_when_start_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let run = StartRun {
        issue_id: Some(1),
        main_job_id: Some(1),
        role: Role::Main,
        phase: "x",
        agent_cmd: "c",
        status_before: None,
        pid: None,
        prompt_path: None,
        log_path: None,
    };
    let res = agent_runs::start(db.pool(), run, TS).await;
    assert!(
        res.is_err(),
        "both issue_id and main_job_id Some must Err (xor guard)"
    );
    Ok(())
}

#[tokio::test]
async fn given_neither_issue_nor_main_job_when_start_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let run = StartRun {
        issue_id: None,
        main_job_id: None,
        role: Role::Main,
        phase: "x",
        agent_cmd: "c",
        status_before: None,
        pid: None,
        prompt_path: None,
        log_path: None,
    };
    let res = agent_runs::start(db.pool(), run, TS).await;
    assert!(res.is_err(), "both None must Err (xor guard)");
    Ok(())
}

#[tokio::test]
async fn given_started_run_when_finished_then_status_after_set() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let id = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    agent_runs::finish(
        db.pool(),
        id,
        Some("PLANNING"),
        Some(0),
        ExitKind::Exited,
        TS2,
        Some("done"),
    )
    .await?;
    let run = agent_runs::get(db.pool(), id).await?.expect("run exists");
    assert_eq!(run.status_after.as_deref(), Some("PLANNING"));
    Ok(())
}

#[tokio::test]
async fn given_started_run_when_finished_then_exit_code_set() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let id = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    agent_runs::finish(
        db.pool(),
        id,
        Some("PLANNING"),
        Some(0),
        ExitKind::Exited,
        TS2,
        Some("done"),
    )
    .await?;
    let run = agent_runs::get(db.pool(), id).await?.expect("run exists");
    assert_eq!(run.exit_code, Some(0));
    Ok(())
}

#[tokio::test]
async fn given_started_run_when_finished_then_exit_kind_set() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let id = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    agent_runs::finish(
        db.pool(),
        id,
        Some("PLANNING"),
        Some(0),
        ExitKind::Exited,
        TS2,
        Some("done"),
    )
    .await?;
    let run = agent_runs::get(db.pool(), id).await?.expect("run exists");
    assert_eq!(run.exit_kind, Some(ExitKind::Exited));
    Ok(())
}

#[tokio::test]
async fn given_started_run_when_finished_then_exited_at_is_supplied_now() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let id = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    agent_runs::finish(
        db.pool(),
        id,
        Some("PLANNING"),
        Some(0),
        ExitKind::Exited,
        TS2,
        Some("done"),
    )
    .await?;
    let run = agent_runs::get(db.pool(), id).await?.expect("run exists");
    assert_eq!(run.exited_at, Some(TS2));
    Ok(())
}

#[tokio::test]
async fn given_started_run_when_finished_then_note_set() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let id = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    agent_runs::finish(
        db.pool(),
        id,
        Some("PLANNING"),
        Some(0),
        ExitKind::Exited,
        TS2,
        Some("done"),
    )
    .await?;
    let run = agent_runs::get(db.pool(), id).await?.expect("run exists");
    assert_eq!(run.note.as_deref(), Some("done"));
    Ok(())
}

#[tokio::test]
async fn given_missing_run_id_when_finish_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let res = agent_runs::finish(db.pool(), 999_999, None, None, ExitKind::Exited, TS2, None).await;
    assert!(res.is_err(), "finish on missing run_id must Err");
    Ok(())
}

#[tokio::test]
async fn given_finish_with_timeout_kind_when_get_then_exit_kind_roundtrips_through_db(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let id = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    agent_runs::finish(db.pool(), id, None, None, ExitKind::Timeout, TS2, None).await?;
    let run = agent_runs::get(db.pool(), id).await?.expect("run exists");
    assert_eq!(run.exit_kind, Some(ExitKind::Timeout));
    Ok(())
}

#[tokio::test]
async fn given_no_run_with_id_when_get_then_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    assert!(agent_runs::get(db.pool(), 999_999).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn given_multiple_runs_for_issue_when_list_by_issue_then_oldest_id_first(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let a = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    let b = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    let c = agent_runs::start(db.pool(), start_for_issue(iid), TS).await?;
    let ids: Vec<i64> = agent_runs::list_by_issue(db.pool(), iid)
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![a, b, c]);
    Ok(())
}

#[tokio::test]
async fn given_multiple_runs_for_issue_when_latest_log_path_then_newest_log_returned(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    let first = start_for_issue(iid);
    agent_runs::start(db.pool(), first, TS).await?;
    let second = StartRun {
        issue_id: Some(iid),
        main_job_id: None,
        role: Role::Work,
        phase: "implement",
        agent_cmd: "claude {prompt}",
        status_before: Some("WORKING"),
        pid: Some(1234),
        prompt_path: Some("/tmp/run2.prompt.txt"),
        log_path: Some("/tmp/run2.log"),
    };
    agent_runs::start(db.pool(), second, TS2).await?;

    let path = agent_runs::latest_log_path_by_issue(db.pool(), iid).await?;

    assert_eq!(path.as_deref(), Some("/tmp/run2.log"));
    Ok(())
}

#[tokio::test]
async fn given_no_runs_for_issue_when_latest_log_path_then_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;

    let path = agent_runs::latest_log_path_by_issue(db.pool(), iid).await?;

    assert_eq!(path, None);
    Ok(())
}

#[tokio::test]
async fn given_runs_in_another_issue_when_list_by_issue_then_excluded() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, mine) = insert_issue(db.pool(), "p1").await?;
    let (_, other) = insert_issue(db.pool(), "p2").await?;
    agent_runs::start(db.pool(), start_for_issue(other), TS).await?;
    let my_run = agent_runs::start(db.pool(), start_for_issue(mine), TS).await?;
    let ids: Vec<i64> = agent_runs::list_by_issue(db.pool(), mine)
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![my_run]);
    Ok(())
}

#[tokio::test]
async fn given_no_runs_for_issue_when_list_by_issue_then_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let (_, iid) = insert_issue(db.pool(), "p").await?;
    assert!(agent_runs::list_by_issue(db.pool(), iid).await?.is_empty());
    Ok(())
}

// ===========================================================================
// Contract B: Role enum round-trip
// ===========================================================================

#[test]
fn given_role_variants_when_roundtripped_then_unchanged() -> anyhow::Result<()> {
    for r in [Role::Main, Role::Plan, Role::Work, Role::Review] {
        assert_eq!(Role::from_str(r.as_str()), Some(r), "{r:?}");
    }
    Ok(())
}

#[test]
fn given_bogus_when_role_from_str_then_none() -> anyhow::Result<()> {
    assert_eq!(Role::from_str("boss"), None);
    Ok(())
}

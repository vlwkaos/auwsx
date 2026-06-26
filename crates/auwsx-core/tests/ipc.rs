//! Integration tests for the auwsx-core IPC layer.
//!
//! Source of truth: crates/auwsx-core/src/ipc.rs (dispatch + transport) plus the
//! CRUD/guard modules it delegates to (backlog, steering, issues, findings).
//!
//! These assert the PUBLIC CONTRACT only:
//!   * `dispatch` never panics; every error path becomes `Response::Err`.
//!   * each Command yields its documented Response variant + emits the documented Event.
//!   * the documented guards: steering working-phase guard (both directions),
//!     SetIssueStatus legal vs illegal (force false vs true, status confirmed via GetIssue).
//!   * transport (serve/request/EventStream/Shutdown/default_socket_path).
//!
//! Failure cases are tested aggressively (missing ids -> Err). Timestamps are
//! injected via the fixed `now` so writes are deterministic.

use auwsx_core::backlog::{self, Approval, Source};
use auwsx_core::db::agent_runs::{self, Role, StartRun};
use auwsx_core::db::arsenal::ArsenalPreset;
use auwsx_core::db::findings::Severity;
use auwsx_core::db::global_settings::{GlobalSettings, PIPELINE_UX_GUIDANCE_MAX_CHARS};
use auwsx_core::db::projects::{self, CompletionPolicy, MergeMode};
use auwsx_core::db::scheduler_runs::{self, SchedulerRunSource};
use auwsx_core::db::{issues, subtasks, Db};
use auwsx_core::events::{self, Event};
use auwsx_core::ipc::{self, Command, Response};
use auwsx_core::routines::RoutineType;
use auwsx_core::state::IssueStatus;
use auwsx_core::steering::SteeringSource;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Fixed deterministic timestamp (epoch ms). No SystemTime in dispatch tests.
const TS: i64 = 1_000_000;

// ---------------------------------------------------------------------------
// Response extractors. Each panics on the wrong variant so a misbehaving
// command surfaces as a test failure at the call site (not a silent skip).
// ---------------------------------------------------------------------------

fn want_id(r: Response) -> i64 {
    match r {
        Response::Id(id) => id,
        other => panic!("expected Response::Id, got {other:?}"),
    }
}

fn want_projects(r: Response) -> Vec<auwsx_core::db::projects::Project> {
    match r {
        Response::Projects(ps) => ps,
        other => panic!("expected Response::Projects, got {other:?}"),
    }
}

fn want_triaged(r: Response) -> Vec<i64> {
    match r {
        Response::Triaged { created_issue_ids } => created_issue_ids,
        other => panic!("expected Response::Triaged, got {other:?}"),
    }
}

fn want_routines(r: Response) -> Vec<auwsx_core::routines::Routine> {
    match r {
        Response::Routines(rs) => rs,
        other => panic!("expected Response::Routines, got {other:?}"),
    }
}

fn want_err(r: Response) -> String {
    match r {
        Response::Err { message } => message,
        other => panic!("expected Response::Err, got {other:?}"),
    }
}

fn want_global_settings(r: Response) -> GlobalSettings {
    match r {
        Response::GlobalSettings(settings) => settings,
        other => panic!("expected Response::GlobalSettings, got {other:?}"),
    }
}

fn is_ok(r: &Response) -> bool {
    matches!(r, Response::Ok)
}

// ---------------------------------------------------------------------------
// Fixtures. A project must exist before backlog/issues (FK). dispatch(AddProject)
// is itself part of the contract, so we set up via dispatch where the spec
// asks for it, and reach for the bus when we need to observe events.
// ---------------------------------------------------------------------------

/// Add a project via dispatch and return its id.
async fn add_project(db: &Db, bus: &tokio::sync::broadcast::Sender<Event>, name: &str) -> i64 {
    want_id(
        ipc::dispatch(
            db,
            bus,
            TS,
            Command::AddProject {
                name: name.to_string(),
                repo_path: "/repo".to_string(),
                default_branch: "main".to_string(),
                arsenal_preset_name: None,
                main_agent_cmd: "m {prompt}".to_string(),
                plan_agent_cmd: "p {prompt}".to_string(),
                work_agent_cmd: "w {prompt}".to_string(),
                review_agent_cmd: None,
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
                schedule_interval_min: None,
                schedule_cron: None,
            },
        )
        .await,
    )
}

/// Seed a project via direct CRUD for transport tests whose subject is not
/// project creation.
async fn backlog_seed_project(db: &Db) -> anyhow::Result<i64> {
    use auwsx_core::db::projects::{self, NewProject};
    projects::create(
        db.pool(),
        NewProject {
            name: "p",
            repo_path: "/r",
            default_branch: "main",
            arsenal_preset_name: None,
            main_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
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

/// Create an issue and force it into `status`, bypassing the transition matrix,
/// so a test can stage a fixture in any phase. Returns the issue id.
async fn issue_at(db: &Db, project_id: i64, status: IssueStatus) -> anyhow::Result<i64> {
    let id = issues::create(db.pool(), project_id, "t", None, TS).await?;
    issues::force_status(db.pool(), id, status, TS).await?;
    Ok(id)
}

fn arsenal_fixture(name: &str) -> ArsenalPreset {
    ArsenalPreset {
        id: 7,
        name: name.to_string(),
        main_agent_cmd: "main {prompt}".to_string(),
        plan_agent_cmd: "plan {prompt}".to_string(),
        work_agent_cmd: "work {prompt}".to_string(),
        review_agent_cmd: Some("review {prompt}".to_string()),
        builtin: false,
        created_at: TS,
        updated_at: TS,
    }
}

// ===========================================================================
// dispatch: ping
// ===========================================================================

#[tokio::test]
async fn given_ping_when_dispatched_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::Ping).await;
    assert!(is_ok(&resp), "Ping must return Ok, got {resp:?}");
    Ok(())
}

#[tokio::test]
async fn given_manual_run_command_without_scheduler_when_dispatched_then_err() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let bus = events::channel();

    let resp = ipc::dispatch(&db, &bus, TS, Command::RunSchedulerOnce { project_id: 1 }).await;

    assert!(
        want_err(resp).contains("daemon runtime"),
        "manual run dispatch without scheduler must explain the missing runtime"
    );
    Ok(())
}

#[test]
fn given_list_arsenal_command_when_json_roundtripped_then_unchanged() -> anyhow::Result<()> {
    let command = Command::ListArsenalPresets;
    let json = serde_json::to_string(&command)?;
    let got: Command = serde_json::from_str(&json)?;
    assert_eq!(got, command);
    Ok(())
}

#[test]
fn given_upsert_arsenal_command_when_json_roundtripped_then_unchanged() -> anyhow::Result<()> {
    let command = Command::UpsertArsenalPreset {
        name: "local".to_string(),
        main_agent_cmd: "main".to_string(),
        plan_agent_cmd: "plan".to_string(),
        work_agent_cmd: "work".to_string(),
        review_agent_cmd: None,
    };
    let json = serde_json::to_string(&command)?;
    let got: Command = serde_json::from_str(&json)?;
    assert_eq!(got, command);
    Ok(())
}

#[test]
fn given_global_settings_commands_when_json_roundtripped_then_unchanged() -> anyhow::Result<()> {
    let commands = vec![
        Command::GetGlobalSettings,
        Command::UpdateGlobalSettings {
            memory_preset_name: "portable-markdown".to_string(),
            pipeline_ux_guidance: "bounded operator guidance".to_string(),
        },
        Command::ListMemoryPresets,
    ];

    for command in commands {
        let json = serde_json::to_string(&command)?;
        let got: Command = serde_json::from_str(&json)?;
        assert_eq!(got, command);
    }
    Ok(())
}

#[test]
fn given_global_settings_response_when_json_roundtripped_then_unchanged() -> anyhow::Result<()> {
    let resp = Response::GlobalSettings(GlobalSettings {
        memory_preset_name: "portable-markdown".to_string(),
        memory_provider: "portable-markdown".to_string(),
        pipeline_ux_guidance: "bounded operator guidance".to_string(),
        updated_at: TS,
    });

    let json = serde_json::to_string(&resp)?;
    let got: Response = serde_json::from_str(&json)?;

    let settings = want_global_settings(got);
    assert_eq!(settings.pipeline_ux_guidance, "bounded operator guidance");
    assert_eq!(settings.updated_at, TS);
    Ok(())
}

#[test]
fn given_arsenal_presets_response_when_json_roundtripped_then_name_is_preserved(
) -> anyhow::Result<()> {
    let expected = arsenal_fixture("local");
    let resp = Response::ArsenalPresets(vec![expected.clone()]);
    let json = serde_json::to_string(&resp)?;
    let got: Response = serde_json::from_str(&json)?;
    match got {
        Response::ArsenalPresets(presets) => {
            assert_eq!(presets.len(), 1);
            let got = &presets[0];
            assert_eq!(got.id, expected.id);
            assert_eq!(got.name, expected.name);
            assert_eq!(got.main_agent_cmd, expected.main_agent_cmd);
            assert_eq!(got.plan_agent_cmd, expected.plan_agent_cmd);
            assert_eq!(got.work_agent_cmd, expected.work_agent_cmd);
            assert_eq!(got.review_agent_cmd, expected.review_agent_cmd);
            assert_eq!(got.builtin, expected.builtin);
            assert_eq!(got.created_at, expected.created_at);
            assert_eq!(got.updated_at, expected.updated_at);
        }
        other => panic!("expected ArsenalPresets, got {other:?}"),
    }
    Ok(())
}

#[test]
fn given_profile_commands_when_json_roundtripped_then_unchanged() -> anyhow::Result<()> {
    let commands = vec![
        Command::ListProfiles,
        Command::CreateProfile {
            name: "ops".to_string(),
        },
        Command::RenameProfile {
            profile_id: 1,
            name: "renamed".to_string(),
        },
        Command::MoveProjectToProfile {
            project_id: 2,
            profile_id: 3,
        },
        Command::MoveProjectInProfile {
            project_id: 4,
            delta: -1,
        },
    ];

    for command in commands {
        let json = serde_json::to_string(&command)?;
        let got: Command = serde_json::from_str(&json)?;
        assert_eq!(got, command);
    }
    Ok(())
}

#[tokio::test]
async fn given_list_arsenal_presets_when_dispatched_then_returns_arsenal_presets(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::ListArsenalPresets).await;
    match resp {
        Response::ArsenalPresets(presets) => assert_eq!(presets.len(), 2),
        other => panic!("expected ArsenalPresets, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_get_global_settings_when_dispatched_then_returns_seeded_settings(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();

    let settings =
        want_global_settings(ipc::dispatch(&db, &bus, TS, Command::GetGlobalSettings).await);

    assert!(
        settings.pipeline_ux_guidance.contains("operator console"),
        "seeded guidance should preserve the auwsx UI standard"
    );
    assert_eq!(settings.updated_at, 0);
    Ok(())
}

#[tokio::test]
async fn given_update_global_settings_when_dispatched_then_persists_trimmed_guidance(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();

    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::UpdateGlobalSettings {
            memory_preset_name: "portable-markdown".to_string(),
            pipeline_ux_guidance: "  no duplicate UI paths\n".to_string(),
        },
    )
    .await;
    assert!(
        is_ok(&resp),
        "UpdateGlobalSettings must return Ok, got {resp:?}"
    );
    let settings =
        want_global_settings(ipc::dispatch(&db, &bus, TS, Command::GetGlobalSettings).await);

    assert_eq!(settings.pipeline_ux_guidance, "no duplicate UI paths");
    assert_eq!(settings.memory_preset_name, "portable-markdown");
    assert_eq!(settings.updated_at, TS);
    Ok(())
}

#[tokio::test]
async fn given_update_global_settings_over_limit_when_dispatched_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let too_long = "x".repeat(PIPELINE_UX_GUIDANCE_MAX_CHARS + 1);

    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::UpdateGlobalSettings {
            memory_preset_name: "portable-markdown".to_string(),
            pipeline_ux_guidance: too_long,
        },
    )
    .await;

    assert!(
        want_err(resp).contains("at most"),
        "overlong guidance must surface as Response::Err"
    );
    Ok(())
}

#[tokio::test]
async fn given_valid_upsert_arsenal_preset_when_dispatched_then_returns_id() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::UpsertArsenalPreset {
            name: "local".to_string(),
            main_agent_cmd: "main".to_string(),
            plan_agent_cmd: "plan".to_string(),
            work_agent_cmd: "work".to_string(),
            review_agent_cmd: None,
        },
    )
    .await;
    assert!(want_id(resp) > 0);
    Ok(())
}

#[tokio::test]
async fn given_list_memory_presets_when_dispatched_then_includes_builtins() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::ListMemoryPresets).await;
    let Response::MemoryPresets(presets) = resp else {
        panic!("expected MemoryPresets response");
    };
    assert!(presets
        .iter()
        .any(|preset| preset.name == "portable-markdown"));
    assert!(presets.iter().any(|preset| preset.name == "auwsx-skills"));
    Ok(())
}

#[tokio::test]
async fn given_valid_upsert_memory_preset_when_dispatched_then_returns_id() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::UpsertMemoryPreset {
            name: "custom-memory".to_string(),
            retrieve_kind: "command".to_string(),
            retrieve_cmd: Some("mem-get {query}".to_string()),
            save_kind: "command".to_string(),
            save_cmd: Some("mem-save {content_file}".to_string()),
            dream_kind: "portable".to_string(),
            dream_cmd: None,
            deepsleep_kind: "portable".to_string(),
            deepsleep_cmd: None,
        },
    )
    .await;
    assert!(want_id(resp) > 0);
    Ok(())
}

#[tokio::test]
async fn given_invalid_upsert_arsenal_preset_when_dispatched_then_returns_err() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::UpsertArsenalPreset {
            name: "local".to_string(),
            main_agent_cmd: " ".to_string(),
            plan_agent_cmd: "plan".to_string(),
            work_agent_cmd: "work".to_string(),
            review_agent_cmd: None,
        },
    )
    .await;
    assert!(want_err(resp).contains("commands are required"));
    Ok(())
}

#[test]
fn given_new_operator_commands_when_json_roundtripped_then_unchanged() -> anyhow::Result<()> {
    let commands = vec![
        Command::UpdateBacklogText {
            item_id: 1,
            text: "edited".to_string(),
        },
        Command::RunSchedulerOnce { project_id: 2 },
        Command::ExecuteProject { project_id: 2 },
        Command::ExecuteIssue { issue_id: 3 },
        Command::RunIssueNow { issue_id: 3 },
        Command::RetryIssue { issue_id: 3 },
        Command::ApproveIssueMerge { issue_id: 3 },
        Command::ApproveProjectMerge { project_id: 2 },
        Command::RunBacklogNow { item_id: 4 },
        Command::RunRoutineNow { routine_id: 5 },
        Command::RemoveProject {
            project_id: 6,
            shallow: true,
        },
        Command::RemoveProject {
            project_id: 6,
            shallow: false,
        },
        Command::CreateRoutine {
            project_id: 7,
            name: "r".to_string(),
            output_route: RoutineType::Knowledge,
            prompt: "p".to_string(),
            cron: "0 0 * * * *".to_string(),
            writable_paths: Some("knowledge/".to_string()),
            enabled: true,
        },
        Command::UpdateRoutine {
            routine_id: 8,
            name: "r2".to_string(),
            output_route: RoutineType::Idea,
            prompt: "p2".to_string(),
            cron: "0 0 * * 1 *".to_string(),
            writable_paths: None,
            enabled: false,
        },
    ];

    for command in commands {
        let json = serde_json::to_string(&command)?;
        let got: Command = serde_json::from_str(&json)?;
        assert_eq!(got, command);
    }
    Ok(())
}

// ===========================================================================
// dispatch: projects
// ===========================================================================

#[tokio::test]
async fn given_valid_add_project_when_dispatched_then_returns_id() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let id = add_project(&db, &bus, "alpha").await;
    assert!(id > 0, "AddProject must return a positive id");
    Ok(())
}

#[tokio::test]
async fn given_duplicate_name_when_add_project_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    add_project(&db, &bus, "dup").await;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddProject {
            name: "dup".to_string(),
            repo_path: "/r".to_string(),
            default_branch: "main".to_string(),
            arsenal_preset_name: None,
            main_agent_cmd: "m".to_string(),
            plan_agent_cmd: "p".to_string(),
            work_agent_cmd: "w".to_string(),
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
    )
    .await;
    let _ = want_err(resp); // UNIQUE(name) violation must become Response::Err, not panic.
    Ok(())
}

#[tokio::test]
async fn given_two_projects_when_list_projects_then_returns_both() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    add_project(&db, &bus, "a").await;
    add_project(&db, &bus, "b").await;
    match ipc::dispatch(&db, &bus, TS, Command::ListProjects).await {
        Response::Projects(ps) => assert_eq!(ps.len(), 2),
        other => panic!("expected Projects, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_existing_project_when_get_project_then_some() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let id = add_project(&db, &bus, "alpha").await;
    match ipc::dispatch(&db, &bus, TS, Command::GetProject { project_id: id }).await {
        Response::Project(p) => assert_eq!(p.expect("present").name, "alpha"),
        other => panic!("expected Project, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_missing_project_when_get_project_then_project_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::GetProject {
            project_id: 999_999,
        },
    )
    .await
    {
        Response::Project(p) => {
            assert_eq!(p, None, "missing project must be Project(None), not Err")
        }
        other => panic!("expected Project, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_update_project_when_dispatched_then_config_fields_change() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let id = add_project(&db, &bus, "configurable").await;

    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::UpdateProject {
            project_id: id,
            name: "configurable-renamed".to_string(),
            repo_path: "/repo2".to_string(),
            default_branch: "trunk".to_string(),
            arsenal_preset_name: None,
            main_agent_cmd: "main2 {prompt}".to_string(),
            plan_agent_cmd: "plan2 {prompt}".to_string(),
            work_agent_cmd: "work2 {prompt}".to_string(),
            review_agent_cmd: Some("review2 {prompt}".to_string()),
            completion_policy: CompletionPolicy::Soft,
            plan_gate_timeout_min: 3,
            completion_soft_timeout_min: 4,
            iteration_timeout_min: 5,
            main_job_timeout_min: 6,
            review_max_rounds: 7,
            conflict_max_attempts: 8,
            max_concurrency: 9,
            schedule_interval_min: Some(10),
            schedule_cron: Some("*/10 * * * *".to_string()),
            merge_mode: MergeMode::Pr,
            skill_path: Some("/skills".to_string()),
            deepsleep_interval_days: 11,
            deepsleep_cron: Some("0 0 */11 * *".to_string()),
        },
    )
    .await;
    assert!(is_ok(&resp), "UpdateProject must return Ok, got {resp:?}");

    let p = projects::get(db.pool(), id).await?.expect("project exists");
    assert_eq!(p.name, "configurable-renamed");
    assert_eq!(p.repo_path, "/repo2");
    assert_eq!(p.default_branch, "trunk");
    assert_eq!(p.review_agent_cmd.as_deref(), Some("review2 {prompt}"));
    assert_eq!(p.completion_policy, CompletionPolicy::Soft);
    assert_eq!(p.schedule_interval_min, Some(10));
    assert_eq!(p.merge_mode, MergeMode::Pr);
    assert_eq!(p.skill_path.as_deref(), Some("/skills"));
    Ok(())
}

// ===========================================================================
// dispatch: backlog
// ===========================================================================

#[tokio::test]
async fn given_human_backlog_when_added_then_returns_id() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog {
            project_id: pid,
            text: "do x".to_string(),
            source: Source::Human,
        },
    )
    .await;
    assert!(want_id(resp) > 0);
    Ok(())
}

#[tokio::test]
async fn given_human_backlog_when_added_then_emits_backlog_changed_approved() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let mut rx = bus.subscribe(); // subscribe BEFORE the emitting dispatch.
    ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog {
            project_id: pid,
            text: "do x".to_string(),
            source: Source::Human,
        },
    )
    .await;
    match rx.try_recv() {
        Ok(Event::BacklogChanged { approval, .. }) => assert_eq!(approval, "approved"),
        other => panic!("expected BacklogChanged(approved), got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_agent_backlog_when_added_then_emits_backlog_changed_pending() -> anyhow::Result<()> {
    // source=agent -> default_approval = Pending -> "pending".
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let mut rx = bus.subscribe();
    ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog {
            project_id: pid,
            text: "y".to_string(),
            source: Source::Agent,
        },
    )
    .await;
    match rx.try_recv() {
        Ok(Event::BacklogChanged { approval, .. }) => assert_eq!(approval, "pending"),
        other => panic!("expected BacklogChanged(pending), got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_unconsumed_backlog_when_update_text_then_text_changes() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let item_id = backlog::add(db.pool(), pid, "old text", Source::Human, None, TS).await?;

    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::UpdateBacklogText {
            item_id,
            text: "new text".to_string(),
        },
    )
    .await;

    assert!(
        is_ok(&resp),
        "UpdateBacklogText must return Ok, got {resp:?}"
    );
    let item = backlog::get(db.pool(), item_id)
        .await?
        .expect("backlog exists");
    assert_eq!(item.text, "new text");
    Ok(())
}

#[tokio::test]
async fn given_consumed_backlog_when_update_text_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let item_id = backlog::add(db.pool(), pid, "old text", Source::Human, None, TS).await?;
    let created =
        want_triaged(ipc::dispatch(&db, &bus, TS, Command::Triage { project_id: pid }).await);
    assert_eq!(created.len(), 1);

    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::UpdateBacklogText {
            item_id,
            text: "new text".to_string(),
        },
    )
    .await;

    assert!(
        want_err(resp).contains("already consumed"),
        "consumed backlog edits must be rejected"
    );
    Ok(())
}

#[tokio::test]
async fn given_mixed_backlog_when_list_filtered_pending_then_only_pending() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    // human -> approved, agent -> pending.
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog {
            project_id: pid,
            text: "h".to_string(),
            source: Source::Human,
        },
    )
    .await;
    let agent_id = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::AddBacklog {
                project_id: pid,
                text: "a".to_string(),
                source: Source::Agent,
            },
        )
        .await,
    );
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListBacklog {
            project_id: pid,
            approval: Some(Approval::Pending),
        },
    )
    .await
    {
        Response::Backlog(items) => {
            let ids: Vec<i64> = items.into_iter().map(|i| i.id).collect();
            assert_eq!(ids, vec![agent_id]);
        }
        other => panic!("expected Backlog, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_no_filter_when_list_backlog_then_returns_all() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog {
            project_id: pid,
            text: "h".to_string(),
            source: Source::Human,
        },
    )
    .await;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog {
            project_id: pid,
            text: "a".to_string(),
            source: Source::Agent,
        },
    )
    .await;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListBacklog {
            project_id: pid,
            approval: None,
        },
    )
    .await
    {
        Response::Backlog(items) => assert_eq!(items.len(), 2),
        other => panic!("expected Backlog, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_pending_item_when_approve_backlog_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::AddBacklog {
                project_id: pid,
                text: "a".to_string(),
                source: Source::Agent,
            },
        )
        .await,
    );
    let resp = ipc::dispatch(&db, &bus, TS, Command::ApproveBacklog { item_id: id }).await;
    assert!(is_ok(&resp), "ApproveBacklog must return Ok, got {resp:?}");
    Ok(())
}

#[tokio::test]
async fn given_approve_backlog_when_dispatched_then_emits_backlog_changed_approved(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::AddBacklog {
                project_id: pid,
                text: "a".to_string(),
                source: Source::Agent,
            },
        )
        .await,
    );
    let mut rx = bus.subscribe(); // after the Add emit, before the Approve emit.
    ipc::dispatch(&db, &bus, TS, Command::ApproveBacklog { item_id: id }).await;
    match rx.try_recv() {
        Ok(Event::BacklogChanged { approval, .. }) => assert_eq!(approval, "approved"),
        other => panic!("expected BacklogChanged(approved), got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_missing_item_when_approve_backlog_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::ApproveBacklog { item_id: 999_999 }).await;
    let _ = want_err(resp);
    Ok(())
}

#[tokio::test]
async fn given_pending_item_when_dismiss_backlog_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::AddBacklog {
                project_id: pid,
                text: "a".to_string(),
                source: Source::Agent,
            },
        )
        .await,
    );
    let resp = ipc::dispatch(&db, &bus, TS, Command::DismissBacklog { item_id: id }).await;
    assert!(is_ok(&resp), "DismissBacklog must return Ok, got {resp:?}");
    Ok(())
}

#[tokio::test]
async fn given_missing_item_when_dismiss_backlog_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::DismissBacklog { item_id: 999_999 }).await;
    let _ = want_err(resp);
    Ok(())
}

// ===========================================================================
// dispatch: triage (via Command::Triage)
// ===========================================================================

#[tokio::test]
async fn given_one_approved_item_when_triage_then_one_issue_created() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog {
            project_id: pid,
            text: "feat".to_string(),
            source: Source::Human,
        },
    )
    .await;
    let created =
        want_triaged(ipc::dispatch(&db, &bus, TS, Command::Triage { project_id: pid }).await);
    assert_eq!(
        created.len(),
        1,
        "one approved item promotes to exactly one issue"
    );
    Ok(())
}

#[tokio::test]
async fn given_only_pending_item_when_triage_then_no_issue_created() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    // agent source stays pending; triage must skip it.
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog {
            project_id: pid,
            text: "x".to_string(),
            source: Source::Agent,
        },
    )
    .await;
    let created =
        want_triaged(ipc::dispatch(&db, &bus, TS, Command::Triage { project_id: pid }).await);
    assert!(created.is_empty(), "pending items are not triaged");
    Ok(())
}

// ===========================================================================
// backlog::run_triage direct (the module behind Command::Triage)
// ===========================================================================

#[tokio::test]
async fn given_approved_item_when_run_triage_then_issue_is_new() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    backlog::add(db.pool(), pid, "feat", Source::Human, None, TS).await?;
    let created = backlog::run_triage(db.pool(), pid, TS).await?;
    let issue = issues::get(db.pool(), created[0])
        .await?
        .expect("created issue exists");
    assert_eq!(issue.status, IssueStatus::New);
    Ok(())
}

#[tokio::test]
async fn given_approved_item_when_run_triage_then_consumed_issue_id_set() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let item_id = backlog::add(db.pool(), pid, "feat", Source::Human, None, TS).await?;
    let created = backlog::run_triage(db.pool(), pid, TS).await?;
    let item = backlog::get(db.pool(), item_id)
        .await?
        .expect("item exists");
    assert_eq!(
        item.consumed_issue_id,
        Some(created[0]),
        "triage links item to its issue"
    );
    Ok(())
}

#[tokio::test]
async fn given_already_consumed_item_when_run_triage_again_then_skipped() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    backlog::add(db.pool(), pid, "feat", Source::Human, None, TS).await?;
    backlog::run_triage(db.pool(), pid, TS).await?; // consumes it
    let second = backlog::run_triage(db.pool(), pid, TS).await?;
    assert!(
        second.is_empty(),
        "an already-consumed item is not re-triaged"
    );
    Ok(())
}

#[tokio::test]
async fn given_pending_item_when_run_triage_then_no_issue() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    backlog::add(db.pool(), pid, "x", Source::Agent, None, TS).await?; // pending
    let created = backlog::run_triage(db.pool(), pid, TS).await?;
    assert!(created.is_empty());
    Ok(())
}

// ===========================================================================
// dispatch: routines
// ===========================================================================

#[tokio::test]
async fn given_create_routine_when_dispatched_then_list_returns_it() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;

    let routine_id = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::CreateRoutine {
                project_id: pid,
                name: "daily".to_string(),
                output_route: RoutineType::Report,
                prompt: "write report".to_string(),
                cron: "0 0 * * * *".to_string(),
                writable_paths: Some("reports/".to_string()),
                enabled: true,
            },
        )
        .await,
    );
    let routines = want_routines(
        ipc::dispatch(&db, &bus, TS, Command::ListRoutines { project_id: pid }).await,
    );

    assert_eq!(routines.len(), 1);
    assert_eq!(routines[0].id, routine_id);
    assert_eq!(routines[0].name, "daily");
    assert_eq!(routines[0].output_route, RoutineType::Report);
    assert!(routines[0].enabled);
    Ok(())
}

#[tokio::test]
async fn given_update_routine_when_dispatched_then_get_returns_updated_fields() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let routine_id = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::CreateRoutine {
                project_id: pid,
                name: "daily".to_string(),
                output_route: RoutineType::Report,
                prompt: "write report".to_string(),
                cron: "0 0 * * * *".to_string(),
                writable_paths: None,
                enabled: true,
            },
        )
        .await,
    );

    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::UpdateRoutine {
            routine_id,
            name: "weekly ideas".to_string(),
            output_route: RoutineType::Idea,
            prompt: "find ideas".to_string(),
            cron: "0 0 * * 1 *".to_string(),
            writable_paths: Some("ideas/".to_string()),
            enabled: false,
        },
    )
    .await;
    assert!(is_ok(&resp), "UpdateRoutine must return Ok, got {resp:?}");

    match ipc::dispatch(&db, &bus, TS, Command::GetRoutine { routine_id }).await {
        Response::Routine(Some(r)) => {
            assert_eq!(r.name, "weekly ideas");
            assert_eq!(r.output_route, RoutineType::Idea);
            assert_eq!(r.prompt, "find ideas");
            assert_eq!(r.cron, "0 0 * * 1 *");
            assert_eq!(r.writable_paths.as_deref(), Some("ideas/"));
            assert!(!r.enabled);
        }
        other => panic!("expected Routine(Some), got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_scheduler_runs_when_recent_requested_then_source_roundtrips() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    scheduler_runs::record(db.pool(), pid, TS, SchedulerRunSource::Auto, Some("{}")).await?;
    scheduler_runs::record(
        db.pool(),
        pid,
        TS + 1,
        SchedulerRunSource::Manual,
        Some("{}"),
    )
    .await?;

    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::RecentSchedulerRunsByProject {
            project_id: pid,
            limit: 10,
        },
    )
    .await
    {
        Response::SchedulerRuns(runs) => {
            assert_eq!(runs.len(), 2);
            assert_eq!(runs[0].source, SchedulerRunSource::Manual);
            assert_eq!(runs[1].source, SchedulerRunSource::Auto);
        }
        other => panic!("expected SchedulerRuns, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_agent_run_log_when_tail_requested_then_reads_recorded_path_only(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let issue_id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let tmp = socket_tempdir()?;
    let log_path = tmp.path().join("agent.log");
    std::fs::write(&log_path, "0123456789abcdef")?;
    let log_str = log_path.to_string_lossy().to_string();
    let run_id = agent_runs::start(
        db.pool(),
        StartRun {
            issue_id: Some(issue_id),
            main_job_id: None,
            role: Role::Main,
            phase: "new",
            agent_cmd: "agent {prompt}",
            status_before: Some("NEW"),
            pid: None,
            prompt_path: None,
            log_path: Some(&log_str),
        },
        TS,
    )
    .await?;

    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::TailAgentRunLog {
            agent_run_id: run_id,
            max_bytes: 4,
        },
    )
    .await
    {
        Response::LogTail { path, text } => {
            assert_eq!(path, log_str);
            assert_eq!(text, "cdef");
        }
        other => panic!("expected LogTail, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_agent_run_without_log_when_tail_requested_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let issue_id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let run_id = agent_runs::start(
        db.pool(),
        StartRun {
            issue_id: Some(issue_id),
            main_job_id: None,
            role: Role::Main,
            phase: "new",
            agent_cmd: "agent {prompt}",
            status_before: Some("NEW"),
            pid: None,
            prompt_path: None,
            log_path: None,
        },
        TS,
    )
    .await?;

    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::TailAgentRunLog {
            agent_run_id: run_id,
            max_bytes: 4,
        },
    )
    .await;

    assert!(
        want_err(resp).contains("has no log_path"),
        "tailing a run without a recorded log path must fail"
    );
    Ok(())
}

// ===========================================================================
// dispatch: issues
// ===========================================================================

#[tokio::test]
async fn given_add_issue_when_dispatched_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddIssue {
            project_id: pid,
            title: "t".to_string(),
            description: None,
        },
    )
    .await;
    let err = want_err(resp);
    assert!(err.contains("agent-derived"));
    Ok(())
}

#[tokio::test]
async fn given_approved_item_when_triage_then_issue_enters_new() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    backlog::add(db.pool(), pid, "t", Source::Human, None, TS).await?;
    let id = match ipc::dispatch(&db, &bus, TS, Command::Triage { project_id: pid }).await {
        Response::Triaged { created_issue_ids } => created_issue_ids[0],
        other => panic!("expected Triaged, got {other:?}"),
    };
    match ipc::dispatch(&db, &bus, TS, Command::GetIssue { issue_id: id }).await {
        Response::Issue(i) => assert_eq!(i.expect("present").status, IssueStatus::New),
        other => panic!("expected Issue, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_get_issue_then_issue_none() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    match ipc::dispatch(&db, &bus, TS, Command::GetIssue { issue_id: 999_999 }).await {
        Response::Issue(i) => assert_eq!(i, None, "missing issue must be Issue(None), not Err"),
        other => panic!("expected Issue, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_mixed_status_issues_when_list_filtered_then_only_that_status() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let planning = issue_at(&db, pid, IssueStatus::Planning).await?;
    issue_at(&db, pid, IssueStatus::Working).await?;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListIssues {
            project_id: pid,
            status: Some(IssueStatus::Planning),
        },
    )
    .await
    {
        Response::Issues(issues) => {
            let ids: Vec<i64> = issues.into_iter().map(|i| i.id).collect();
            assert_eq!(ids, vec![planning]);
        }
        other => panic!("expected Issues, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_no_status_filter_when_list_issues_then_returns_all() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    issue_at(&db, pid, IssueStatus::Planning).await?;
    issue_at(&db, pid, IssueStatus::Working).await?;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListIssues {
            project_id: pid,
            status: None,
        },
    )
    .await
    {
        Response::Issues(issues) => assert_eq!(issues.len(), 2),
        other => panic!("expected Issues, got {other:?}"),
    }
    Ok(())
}

// --- SetIssueStatus: legal / illegal / force / event ----------------------

#[tokio::test]
async fn given_new_when_set_status_planning_then_ok() -> anyhow::Result<()> {
    // NEW -> PLANNING is a legal transition.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::SetIssueStatus {
            issue_id: id,
            status: IssueStatus::Planning,
            force: false,
        },
    )
    .await;
    assert!(
        is_ok(&resp),
        "legal transition must return Ok, got {resp:?}"
    );
    Ok(())
}

#[tokio::test]
async fn given_legal_set_status_when_dispatched_then_emits_issue_status() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let mut rx = bus.subscribe();
    ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::SetIssueStatus {
            issue_id: id,
            status: IssueStatus::Planning,
            force: false,
        },
    )
    .await;
    match rx.try_recv() {
        Ok(Event::IssueStatus { status, .. }) => assert_eq!(status, IssueStatus::Planning),
        other => panic!("expected IssueStatus(Planning), got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_illegal_set_status_unforced_when_dispatched_then_err() -> anyhow::Result<()> {
    // NEW -> DONE is illegal; force=false must reject.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::SetIssueStatus {
            issue_id: id,
            status: IssueStatus::Done,
            force: false,
        },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

#[tokio::test]
async fn given_illegal_set_status_unforced_when_dispatched_then_status_unchanged(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::SetIssueStatus {
            issue_id: id,
            status: IssueStatus::Done,
            force: false,
        },
    )
    .await;
    // Confirm via GetIssue that the rejected transition left status untouched.
    match ipc::dispatch(&db, &bus, TS, Command::GetIssue { issue_id: id }).await {
        Response::Issue(i) => {
            assert_eq!(i.expect("present").status, IssueStatus::New)
        }
        other => panic!("expected Issue, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_illegal_set_status_forced_when_dispatched_then_ok() -> anyhow::Result<()> {
    // force=true bypasses the legality matrix.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::SetIssueStatus {
            issue_id: id,
            status: IssueStatus::Done,
            force: true,
        },
    )
    .await;
    assert!(
        is_ok(&resp),
        "forced transition must return Ok, got {resp:?}"
    );
    Ok(())
}

#[tokio::test]
async fn given_illegal_set_status_forced_when_dispatched_then_status_changed() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::SetIssueStatus {
            issue_id: id,
            status: IssueStatus::Done,
            force: true,
        },
    )
    .await;
    match ipc::dispatch(&db, &bus, TS, Command::GetIssue { issue_id: id }).await {
        Response::Issue(i) => assert_eq!(i.expect("present").status, IssueStatus::Done),
        other => panic!("expected Issue, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_missing_issue_when_set_status_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::SetIssueStatus {
            issue_id: 999_999,
            status: IssueStatus::Planning,
            force: false,
        },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

// ===========================================================================
// dispatch: subtasks
// ===========================================================================

#[tokio::test]
async fn given_add_subtask_when_dispatched_then_returns_id() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Working).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSubtask {
            issue_id: iid,
            ord: 0,
            text: "step".to_string(),
        },
    )
    .await;
    assert!(want_id(resp) > 0);
    Ok(())
}

#[tokio::test]
async fn given_subtask_added_when_list_subtasks_then_returns_it() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Working).await?;
    let _ = subtasks::add(db.pool(), iid, 0, "step", TS).await?;
    match ipc::dispatch(&db, &bus, TS, Command::ListSubtasks { issue_id: iid }).await {
        Response::Subtasks(subs) => assert_eq!(subs.len(), 1),
        other => panic!("expected Subtasks, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_subtask_when_complete_subtask_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Working).await?;
    let sid = subtasks::add(db.pool(), iid, 0, "step", TS).await?;
    let resp = ipc::dispatch(&db, &bus, TS, Command::CompleteSubtask { subtask_id: sid }).await;
    assert!(is_ok(&resp), "CompleteSubtask must return Ok, got {resp:?}");
    Ok(())
}

#[tokio::test]
async fn given_missing_subtask_when_complete_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::CompleteSubtask {
            subtask_id: 999_999,
        },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

// ===========================================================================
// dispatch: findings
// ===========================================================================

/// Add a minimal finding to `issue_id` via dispatch, returning its id.
async fn add_finding(db: &Db, bus: &tokio::sync::broadcast::Sender<Event>, issue_id: i64) -> i64 {
    want_id(
        ipc::dispatch(
            db,
            bus,
            TS,
            Command::AddFinding {
                issue_id,
                review_round: 0,
                severity: Severity::Major,
                lens: None,
                title: "f".to_string(),
                detail: None,
                file_ref: None,
            },
        )
        .await,
    )
}

#[tokio::test]
async fn given_add_finding_when_dispatched_then_returns_id() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Reviewing).await?;
    let fid = add_finding(&db, &bus, iid).await;
    assert!(fid > 0);
    Ok(())
}

#[tokio::test]
async fn given_add_finding_when_dispatched_then_emits_finding_added() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Reviewing).await?;
    let mut rx = bus.subscribe();
    let fid = add_finding(&db, &bus, iid).await;
    match rx.try_recv() {
        Ok(Event::FindingAdded {
            finding_id,
            issue_id,
        }) => {
            assert_eq!((finding_id, issue_id), (fid, iid))
        }
        other => panic!("expected FindingAdded, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_two_findings_when_list_findings_all_then_returns_both() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Reviewing).await?;
    add_finding(&db, &bus, iid).await;
    add_finding(&db, &bus, iid).await;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListFindings {
            issue_id: iid,
            open_only: false,
        },
    )
    .await
    {
        Response::Findings(fs) => assert_eq!(fs.len(), 2),
        other => panic!("expected Findings, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_accepted_finding_when_list_open_only_then_excluded() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Reviewing).await?;
    let open = add_finding(&db, &bus, iid).await;
    let accepted = add_finding(&db, &bus, iid).await;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AcceptFinding {
            finding_id: accepted,
            rationale: "ok".to_string(),
        },
    )
    .await;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListFindings {
            issue_id: iid,
            open_only: true,
        },
    )
    .await
    {
        Response::Findings(fs) => {
            let ids: Vec<i64> = fs.into_iter().map(|f| f.id).collect();
            assert_eq!(ids, vec![open]);
        }
        other => panic!("expected Findings, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_accept_finding_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Reviewing).await?;
    let fid = add_finding(&db, &bus, iid).await;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AcceptFinding {
            finding_id: fid,
            rationale: "ok".to_string(),
        },
    )
    .await;
    assert!(is_ok(&resp), "AcceptFinding must return Ok, got {resp:?}");
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_reject_finding_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Reviewing).await?;
    let fid = add_finding(&db, &bus, iid).await;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::RejectFinding {
            finding_id: fid,
            rationale: "fp".to_string(),
        },
    )
    .await;
    assert!(is_ok(&resp), "RejectFinding must return Ok, got {resp:?}");
    Ok(())
}

#[tokio::test]
async fn given_open_finding_when_dismiss_finding_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Reviewing).await?;
    let fid = add_finding(&db, &bus, iid).await;
    let resp = ipc::dispatch(&db, &bus, TS, Command::DismissFinding { finding_id: fid }).await;
    assert!(is_ok(&resp), "DismissFinding must return Ok, got {resp:?}");
    Ok(())
}

#[tokio::test]
async fn given_missing_finding_when_accept_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AcceptFinding {
            finding_id: 999_999,
            rationale: "x".to_string(),
        },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

#[tokio::test]
async fn given_missing_finding_when_dismiss_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::DismissFinding {
            finding_id: 999_999,
        },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

// ===========================================================================
// dispatch: steering (the working-phase guard is the headline contract)
// ===========================================================================

#[tokio::test]
async fn given_working_issue_when_add_steering_then_returns_id() -> anyhow::Result<()> {
    // WORKING accepts steering -> Ok with an id.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Working).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering {
            issue_id: iid,
            source: SteeringSource::Human,
            note: "n".to_string(),
        },
    )
    .await;
    assert!(want_id(resp) > 0);
    Ok(())
}

#[tokio::test]
async fn given_ready_to_merge_issue_when_add_steering_then_returns_id() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::ReadyToMerge).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering {
            issue_id: iid,
            source: SteeringSource::Human,
            note: "verify one more case before merge".to_string(),
        },
    )
    .await;
    assert!(want_id(resp) > 0);
    Ok(())
}

#[tokio::test]
async fn given_add_steering_when_accepted_then_emits_steering_added() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Working).await?;
    let mut rx = bus.subscribe();
    let sid = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::AddSteering {
                issue_id: iid,
                source: SteeringSource::Human,
                note: "n".to_string(),
            },
        )
        .await,
    );
    match rx.try_recv() {
        Ok(Event::SteeringAdded {
            steering_id,
            issue_id,
        }) => {
            assert_eq!((steering_id, issue_id), (sid, iid))
        }
        other => panic!("expected SteeringAdded, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_new_issue_when_add_steering_then_err() -> anyhow::Result<()> {
    // NEW does NOT accept steering (no locked plan yet) -> guard Err.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issues::create(db.pool(), pid, "t", None, TS).await?; // NEW
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering {
            issue_id: iid,
            source: SteeringSource::Human,
            note: "n".to_string(),
        },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

#[tokio::test]
async fn given_plan_ready_issue_when_add_steering_then_err() -> anyhow::Result<()> {
    // PLAN_READY is a real status but not a working phase -> guard Err.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::PlanReady).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering {
            issue_id: iid,
            source: SteeringSource::Human,
            note: "n".to_string(),
        },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

#[tokio::test]
async fn given_rejected_steering_with_subscriber_then_no_event_emitted() -> anyhow::Result<()> {
    // The guard must reject BEFORE emitting: a failed AddSteering broadcasts nothing.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issues::create(db.pool(), pid, "t", None, TS).await?; // NEW
    let mut rx = bus.subscribe();
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering {
            issue_id: iid,
            source: SteeringSource::Human,
            note: "n".to_string(),
        },
    )
    .await;
    // Event is not PartialEq, so match the Result rather than assert_eq! on it.
    assert!(
        matches!(rx.try_recv(), Err(TryRecvError::Empty)),
        "rejected steering must emit no event"
    );
    Ok(())
}

#[tokio::test]
async fn given_pending_steering_when_list_steering_then_returns_it() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Working).await?;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering {
            issue_id: iid,
            source: SteeringSource::Human,
            note: "n".to_string(),
        },
    )
    .await;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListSteering {
            issue_id: iid,
            pending_only: true,
        },
    )
    .await
    {
        Response::Steering(s) => assert_eq!(s.len(), 1),
        other => panic!("expected Steering, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_pending_steering_when_consume_steering_then_list_empty() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Working).await?;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering {
            issue_id: iid,
            source: SteeringSource::Human,
            note: "n".to_string(),
        },
    )
    .await;
    let consume = ipc::dispatch(&db, &bus, TS, Command::ConsumeSteering { issue_id: iid }).await;
    assert!(
        is_ok(&consume),
        "ConsumeSteering must return Ok, got {consume:?}"
    );
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListSteering {
            issue_id: iid,
            pending_only: true,
        },
    )
    .await
    {
        Response::Steering(s) => assert!(s.is_empty(), "consumed steering is no longer pending"),
        other => panic!("expected Steering, got {other:?}"),
    }
    Ok(())
}

// ===========================================================================
// dispatch: lifecycle no-ops (Subscribe/Shutdown via dispatch alone)
// ===========================================================================

#[tokio::test]
async fn given_subscribe_via_dispatch_when_called_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::Subscribe).await;
    assert!(
        is_ok(&resp),
        "Subscribe via dispatch alone is a no-op Ok, got {resp:?}"
    );
    Ok(())
}

#[tokio::test]
async fn given_shutdown_via_dispatch_when_called_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::Shutdown).await;
    assert!(
        is_ok(&resp),
        "Shutdown via dispatch alone is a no-op Ok, got {resp:?}"
    );
    Ok(())
}

// ===========================================================================
// transport: serve / request / EventStream / Shutdown
// ===========================================================================

async fn wait_for_server_socket(
    path: &Path,
    server: &mut JoinHandle<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    for _ in 0..200 {
        if path.exists() {
            return Ok(());
        }
        if server.is_finished() {
            let result = server.await?;
            result?;
            anyhow::bail!("server exited before socket {} appeared", path.display());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    anyhow::bail!("socket {} never appeared", path.display())
}

fn socket_tempdir() -> anyhow::Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .prefix("ipc-socket-")
        .tempdir_in(std::env::current_dir()?)?)
}

#[tokio::test]
async fn given_running_server_when_request_ping_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let tmp = socket_tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let mut server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_server_socket(&sock, &mut server).await?;

    let resp = ipc::request(&sock, &Command::Ping).await?;
    assert!(is_ok(&resp), "request(Ping) must return Ok, got {resp:?}");

    shutdown.notify_one();
    server.await??;
    Ok(())
}

/// CONTRACT: a command returning `Response::Id` over the real socket must yield
/// that id to the client. `Response` uses adjacent tagging
/// (`#[serde(tag = "kind", content = "data")]`), under which the newtype variant
/// `Id(i64)` serializes cleanly, so the round-trip succeeds.
#[tokio::test]
async fn given_running_server_when_request_add_project_then_id() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let tmp = socket_tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let mut server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_server_socket(&sock, &mut server).await?;

    let resp = ipc::request(
        &sock,
        &Command::AddProject {
            name: "alpha".to_string(),
            repo_path: "/r".to_string(),
            default_branch: "main".to_string(),
            arsenal_preset_name: None,
            main_agent_cmd: "m".to_string(),
            plan_agent_cmd: "p".to_string(),
            work_agent_cmd: "w".to_string(),
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
    )
    .await?;
    assert!(want_id(resp) > 0);

    shutdown.notify_one();
    server.await??;
    Ok(())
}

/// Vec newtype variant (`Projects(Vec<_>)`) over the wire: after seeding one
/// project via the socket, ListProjects must round-trip a one-element Vec.
#[tokio::test]
async fn given_running_server_when_request_list_projects_then_projects_vec() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let tmp = socket_tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let mut server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_server_socket(&sock, &mut server).await?;

    let added = ipc::request(
        &sock,
        &Command::AddProject {
            name: "alpha".to_string(),
            repo_path: "/r".to_string(),
            default_branch: "main".to_string(),
            arsenal_preset_name: None,
            main_agent_cmd: "m".to_string(),
            plan_agent_cmd: "p".to_string(),
            work_agent_cmd: "w".to_string(),
            review_agent_cmd: None,
            completion_policy: None,
            plan_gate_timeout_min: None,
            completion_soft_timeout_min: None,
            schedule_interval_min: None,
            schedule_cron: None,
        },
    )
    .await?;
    assert!(want_id(added) > 0);

    let resp = ipc::request(&sock, &Command::ListProjects).await?;
    assert_eq!(
        want_projects(resp).len(),
        1,
        "one seeded project must round-trip as a one-element Vec"
    );

    shutdown.notify_one();
    server.await??;
    Ok(())
}

/// Option=None newtype variant (`Project(Option<_>)`) over the wire: a missing
/// project must round-trip as `Project(None)`, not an Err or a dropped conn.
#[tokio::test]
async fn given_running_server_when_request_get_missing_project_then_project_none(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let tmp = socket_tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let mut server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_server_socket(&sock, &mut server).await?;

    let resp = ipc::request(
        &sock,
        &Command::GetProject {
            project_id: 999_999,
        },
    )
    .await?;
    match resp {
        Response::Project(p) => {
            assert_eq!(p, None, "missing project must round-trip as Project(None)")
        }
        other => panic!("expected Project, got {other:?}"),
    }

    shutdown.notify_one();
    server.await??;
    Ok(())
}

/// Empty Vec newtype variant (`Issues(Vec<_>)`) over the wire: a project with no
/// issues must round-trip ListIssues as an empty Vec.
#[tokio::test]
async fn given_running_server_when_request_list_issues_empty_then_empty_vec() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = backlog_seed_project(&db).await?; // seed off-socket; subject is the Issues round-trip.

    let tmp = socket_tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let mut server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_server_socket(&sock, &mut server).await?;

    let resp = ipc::request(
        &sock,
        &Command::ListIssues {
            project_id: pid,
            status: None,
        },
    )
    .await?;
    match resp {
        Response::Issues(v) => assert!(v.is_empty(), "no issues must round-trip as an empty Vec"),
        other => panic!("expected Issues, got {other:?}"),
    }

    shutdown.notify_one();
    server.await??;
    Ok(())
}

#[tokio::test]
async fn given_running_server_when_shutdown_request_then_server_task_completes(
) -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let tmp = socket_tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let mut server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_server_socket(&sock, &mut server).await?;

    let resp = ipc::request(&sock, &Command::Shutdown).await?;
    assert!(
        is_ok(&resp),
        "Shutdown request must return Ok, got {resp:?}"
    );
    // serve() must return Ok after the Shutdown command triggers the notify.
    server.await??;
    Ok(())
}

#[tokio::test]
async fn given_raw_bad_line_when_sent_then_response_is_err() -> anyhow::Result<()> {
    // request() always serializes a valid Command, so to exercise the malformed
    // line path we write raw bytes over a UnixStream.
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let db = Db::open_memory().await?;
    let bus = events::channel();
    let tmp = socket_tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let mut server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_server_socket(&sock, &mut server).await?;

    let mut stream = UnixStream::connect(&sock).await?;
    stream.write_all(b"not json\n").await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let resp: Response = serde_json::from_str(&line)?;
    let _ = want_err(resp); // malformed line yields a Response::Err line.

    shutdown.notify_one();
    server.await??;
    Ok(())
}

#[tokio::test]
async fn given_event_stream_when_state_changes_then_yields_backlog_changed() -> anyhow::Result<()> {
    // This test's subject is the EventStream subscription. Both the project
    // and a pending backlog item are seeded via direct CRUD on the shared pool
    // so setup does not add unrelated events to the stream.
    // The emitting command is ApproveBacklog: it returns Response::Ok (which
    // serializes cleanly) and broadcasts Event::BacklogChanged.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = backlog_seed_project(&db).await?;
    let item_id = backlog::add(db.pool(), pid, "x", Source::Agent, None, TS).await?; // pending

    let tmp = socket_tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let mut server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_server_socket(&sock, &mut server).await?;

    // Open the subscription BEFORE the emitting command.
    let mut stream = ipc::EventStream::connect(&sock).await?;
    let resp = ipc::request(&sock, &Command::ApproveBacklog { item_id }).await?;
    assert!(
        is_ok(&resp),
        "ApproveBacklog over socket must return Ok, got {resp:?}"
    );

    match stream.next().await? {
        Some(Event::BacklogChanged { project_id, .. }) => assert_eq!(project_id, pid),
        other => panic!("expected BacklogChanged event, got {other:?}"),
    }

    shutdown.notify_one();
    server.await??;
    Ok(())
}

// ===========================================================================
// default_socket_path: $AUWSX_SOCK override (process-global env; isolated test)
// ===========================================================================

#[tokio::test]
async fn given_auwsx_sock_env_set_when_default_socket_path_then_returns_it() -> anyhow::Result<()> {
    // env is process-global: set + read + unset within this one test.
    std::env::set_var("AUWSX_SOCK", "/tmp/auwsx-test-override.sock");
    let got = ipc::default_socket_path();
    std::env::remove_var("AUWSX_SOCK");
    assert_eq!(got, Path::new("/tmp/auwsx-test-override.sock"));
    Ok(())
}

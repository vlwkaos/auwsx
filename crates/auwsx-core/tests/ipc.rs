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
use auwsx_core::db::findings::Severity;
use auwsx_core::db::{issues, Db};
use auwsx_core::events::{self, Event};
use auwsx_core::ipc::{self, Command, Response};
use auwsx_core::state::IssueStatus;
use auwsx_core::steering::SteeringSource;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::Notify;

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

fn want_err(r: Response) -> String {
    match r {
        Response::Err { message } => message,
        other => panic!("expected Response::Err, got {other:?}"),
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
                main_agent_cmd: "m {prompt}".to_string(),
                plan_agent_cmd: "p {prompt}".to_string(),
                work_agent_cmd: "w {prompt}".to_string(),
                review_agent_cmd: None,
            },
        )
        .await,
    )
}

/// Seed a project via direct CRUD (no socket / no `Response::Id`), for transport
/// tests whose subject is NOT Id serialization.
async fn backlog_seed_project(db: &Db) -> anyhow::Result<i64> {
    use auwsx_core::db::projects::{self, NewProject};
    projects::create(
        db.pool(),
        NewProject {
            name: "p",
            repo_path: "/r",
            default_branch: "main",
            main_agent_cmd: "m",
            plan_agent_cmd: "p",
            work_agent_cmd: "w",
            review_agent_cmd: None,
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
            main_agent_cmd: "m".to_string(),
            plan_agent_cmd: "p".to_string(),
            work_agent_cmd: "w".to_string(),
            review_agent_cmd: None,
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
    match ipc::dispatch(&db, &bus, TS, Command::GetProject { project_id: 999_999 }).await {
        Response::Project(p) => assert_eq!(p, None, "missing project must be Project(None), not Err"),
        other => panic!("expected Project, got {other:?}"),
    }
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
        Command::AddBacklog { project_id: pid, text: "do x".to_string(), source: Source::Human },
    )
    .await;
    assert!(want_id(resp) > 0);
    Ok(())
}

#[tokio::test]
async fn given_human_backlog_when_added_then_emits_backlog_changed_approved() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let mut rx = bus.subscribe(); // subscribe BEFORE the emitting dispatch.
    ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog { project_id: pid, text: "do x".to_string(), source: Source::Human },
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
        Command::AddBacklog { project_id: pid, text: "y".to_string(), source: Source::Agent },
    )
    .await;
    match rx.try_recv() {
        Ok(Event::BacklogChanged { approval, .. }) => assert_eq!(approval, "pending"),
        other => panic!("expected BacklogChanged(pending), got {other:?}"),
    }
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
        Command::AddBacklog { project_id: pid, text: "h".to_string(), source: Source::Human },
    )
    .await;
    let agent_id = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::AddBacklog { project_id: pid, text: "a".to_string(), source: Source::Agent },
        )
        .await,
    );
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListBacklog { project_id: pid, approval: Some(Approval::Pending) },
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
        Command::AddBacklog { project_id: pid, text: "h".to_string(), source: Source::Human },
    )
    .await;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddBacklog { project_id: pid, text: "a".to_string(), source: Source::Agent },
    )
    .await;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListBacklog { project_id: pid, approval: None },
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
            Command::AddBacklog { project_id: pid, text: "a".to_string(), source: Source::Agent },
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
            Command::AddBacklog { project_id: pid, text: "a".to_string(), source: Source::Agent },
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
            Command::AddBacklog { project_id: pid, text: "a".to_string(), source: Source::Agent },
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
        Command::AddBacklog { project_id: pid, text: "feat".to_string(), source: Source::Human },
    )
    .await;
    let created = want_triaged(ipc::dispatch(&db, &bus, TS, Command::Triage { project_id: pid }).await);
    assert_eq!(created.len(), 1, "one approved item promotes to exactly one issue");
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
        Command::AddBacklog { project_id: pid, text: "x".to_string(), source: Source::Agent },
    )
    .await;
    let created = want_triaged(ipc::dispatch(&db, &bus, TS, Command::Triage { project_id: pid }).await);
    assert!(created.is_empty(), "pending items are not triaged");
    Ok(())
}

// ===========================================================================
// backlog::run_triage direct (the module behind Command::Triage)
// ===========================================================================

#[tokio::test]
async fn given_approved_item_when_run_triage_then_issue_is_consolidating() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    backlog::add(db.pool(), pid, "feat", Source::Human, None, TS).await?;
    let created = backlog::run_triage(db.pool(), pid, TS).await?;
    let issue = issues::get(db.pool(), created[0]).await?.expect("created issue exists");
    assert_eq!(issue.status, IssueStatus::Consolidating);
    Ok(())
}

#[tokio::test]
async fn given_approved_item_when_run_triage_then_consumed_issue_id_set() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let item_id = backlog::add(db.pool(), pid, "feat", Source::Human, None, TS).await?;
    let created = backlog::run_triage(db.pool(), pid, TS).await?;
    let item = backlog::get(db.pool(), item_id).await?.expect("item exists");
    assert_eq!(item.consumed_issue_id, Some(created[0]), "triage links item to its issue");
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
    assert!(second.is_empty(), "an already-consumed item is not re-triaged");
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
// dispatch: issues
// ===========================================================================

#[tokio::test]
async fn given_add_issue_when_dispatched_then_returns_id() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddIssue { project_id: pid, title: "t".to_string(), description: None },
    )
    .await;
    assert!(want_id(resp) > 0);
    Ok(())
}

#[tokio::test]
async fn given_add_issue_when_dispatched_then_issue_enters_consolidating() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::AddIssue { project_id: pid, title: "t".to_string(), description: None },
        )
        .await,
    );
    match ipc::dispatch(&db, &bus, TS, Command::GetIssue { issue_id: id }).await {
        Response::Issue(i) => assert_eq!(i.expect("present").status, IssueStatus::Consolidating),
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
async fn given_mixed_status_issues_when_list_filtered_then_only_that_status() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let planning = issue_at(&db, pid, IssueStatus::Planning).await?;
    issue_at(&db, pid, IssueStatus::Implementing).await?;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListIssues { project_id: pid, status: Some(IssueStatus::Planning) },
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
    issue_at(&db, pid, IssueStatus::Implementing).await?;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListIssues { project_id: pid, status: None },
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
async fn given_consolidating_when_set_status_planning_then_ok() -> anyhow::Result<()> {
    // CONSOLIDATING -> PLANNING is a legal transition.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::SetIssueStatus { issue_id: id, status: IssueStatus::Planning, force: false },
    )
    .await;
    assert!(is_ok(&resp), "legal transition must return Ok, got {resp:?}");
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
        Command::SetIssueStatus { issue_id: id, status: IssueStatus::Planning, force: false },
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
    // CONSOLIDATING -> DONE is illegal; force=false must reject.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let id = issues::create(db.pool(), pid, "t", None, TS).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::SetIssueStatus { issue_id: id, status: IssueStatus::Done, force: false },
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
        Command::SetIssueStatus { issue_id: id, status: IssueStatus::Done, force: false },
    )
    .await;
    // Confirm via GetIssue that the rejected transition left status untouched.
    match ipc::dispatch(&db, &bus, TS, Command::GetIssue { issue_id: id }).await {
        Response::Issue(i) => {
            assert_eq!(i.expect("present").status, IssueStatus::Consolidating)
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
        Command::SetIssueStatus { issue_id: id, status: IssueStatus::Done, force: true },
    )
    .await;
    assert!(is_ok(&resp), "forced transition must return Ok, got {resp:?}");
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
        Command::SetIssueStatus { issue_id: id, status: IssueStatus::Done, force: true },
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
        Command::SetIssueStatus { issue_id: 999_999, status: IssueStatus::Planning, force: false },
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
    let iid = issue_at(&db, pid, IssueStatus::Implementing).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSubtask { issue_id: iid, ord: 0, text: "step".to_string() },
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
    let iid = issue_at(&db, pid, IssueStatus::Implementing).await?;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSubtask { issue_id: iid, ord: 0, text: "step".to_string() },
    )
    .await;
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
    let iid = issue_at(&db, pid, IssueStatus::Implementing).await?;
    let sid = want_id(
        ipc::dispatch(
            &db,
            &bus,
            TS,
            Command::AddSubtask { issue_id: iid, ord: 0, text: "step".to_string() },
        )
        .await,
    );
    let resp = ipc::dispatch(&db, &bus, TS, Command::CompleteSubtask { subtask_id: sid }).await;
    assert!(is_ok(&resp), "CompleteSubtask must return Ok, got {resp:?}");
    Ok(())
}

#[tokio::test]
async fn given_missing_subtask_when_complete_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::CompleteSubtask { subtask_id: 999_999 }).await;
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
    let iid = issue_at(&db, pid, IssueStatus::Review).await?;
    let fid = add_finding(&db, &bus, iid).await;
    assert!(fid > 0);
    Ok(())
}

#[tokio::test]
async fn given_add_finding_when_dispatched_then_emits_finding_added() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Review).await?;
    let mut rx = bus.subscribe();
    let fid = add_finding(&db, &bus, iid).await;
    match rx.try_recv() {
        Ok(Event::FindingAdded { finding_id, issue_id }) => {
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
    let iid = issue_at(&db, pid, IssueStatus::Review).await?;
    add_finding(&db, &bus, iid).await;
    add_finding(&db, &bus, iid).await;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListFindings { issue_id: iid, open_only: false },
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
    let iid = issue_at(&db, pid, IssueStatus::Review).await?;
    let open = add_finding(&db, &bus, iid).await;
    let accepted = add_finding(&db, &bus, iid).await;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AcceptFinding { finding_id: accepted, rationale: "ok".to_string() },
    )
    .await;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListFindings { issue_id: iid, open_only: true },
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
    let iid = issue_at(&db, pid, IssueStatus::Review).await?;
    let fid = add_finding(&db, &bus, iid).await;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AcceptFinding { finding_id: fid, rationale: "ok".to_string() },
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
    let iid = issue_at(&db, pid, IssueStatus::Review).await?;
    let fid = add_finding(&db, &bus, iid).await;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::RejectFinding { finding_id: fid, rationale: "fp".to_string() },
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
    let iid = issue_at(&db, pid, IssueStatus::Review).await?;
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
        Command::AcceptFinding { finding_id: 999_999, rationale: "x".to_string() },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

#[tokio::test]
async fn given_missing_finding_when_dismiss_then_err() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::DismissFinding { finding_id: 999_999 }).await;
    let _ = want_err(resp);
    Ok(())
}

// ===========================================================================
// dispatch: steering (the working-phase guard is the headline contract)
// ===========================================================================

#[tokio::test]
async fn given_implementing_issue_when_add_steering_then_returns_id() -> anyhow::Result<()> {
    // IMPLEMENTING accepts steering -> Ok with an id.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Implementing).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering { issue_id: iid, source: SteeringSource::Human, note: "n".to_string() },
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
    let iid = issue_at(&db, pid, IssueStatus::Implementing).await?;
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
        Ok(Event::SteeringAdded { steering_id, issue_id }) => {
            assert_eq!((steering_id, issue_id), (sid, iid))
        }
        other => panic!("expected SteeringAdded, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn given_consolidating_issue_when_add_steering_then_err() -> anyhow::Result<()> {
    // CONSOLIDATING does NOT accept steering (no locked plan yet) -> guard Err.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issues::create(db.pool(), pid, "t", None, TS).await?; // CONSOLIDATING
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering { issue_id: iid, source: SteeringSource::Human, note: "n".to_string() },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

#[tokio::test]
async fn given_planned_issue_when_add_steering_then_err() -> anyhow::Result<()> {
    // PLANNED is a real status but not a working phase -> guard Err.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issue_at(&db, pid, IssueStatus::Planned).await?;
    let resp = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering { issue_id: iid, source: SteeringSource::Human, note: "n".to_string() },
    )
    .await;
    let _ = want_err(resp);
    Ok(())
}

#[tokio::test]
async fn given_rejected_steering_when_no_subscriber_then_no_event_emitted() -> anyhow::Result<()> {
    // The guard must reject BEFORE emitting: a failed AddSteering broadcasts nothing.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = add_project(&db, &bus, "p").await;
    let iid = issues::create(db.pool(), pid, "t", None, TS).await?; // CONSOLIDATING
    let mut rx = bus.subscribe();
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering { issue_id: iid, source: SteeringSource::Human, note: "n".to_string() },
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
    let iid = issue_at(&db, pid, IssueStatus::Implementing).await?;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering { issue_id: iid, source: SteeringSource::Human, note: "n".to_string() },
    )
    .await;
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListSteering { issue_id: iid, pending_only: true },
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
    let iid = issue_at(&db, pid, IssueStatus::Implementing).await?;
    let _ = ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::AddSteering { issue_id: iid, source: SteeringSource::Human, note: "n".to_string() },
    )
    .await;
    let consume = ipc::dispatch(&db, &bus, TS, Command::ConsumeSteering { issue_id: iid }).await;
    assert!(is_ok(&consume), "ConsumeSteering must return Ok, got {consume:?}");
    match ipc::dispatch(
        &db,
        &bus,
        TS,
        Command::ListSteering { issue_id: iid, pending_only: true },
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
    assert!(is_ok(&resp), "Subscribe via dispatch alone is a no-op Ok, got {resp:?}");
    Ok(())
}

#[tokio::test]
async fn given_shutdown_via_dispatch_when_called_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let resp = ipc::dispatch(&db, &bus, TS, Command::Shutdown).await;
    assert!(is_ok(&resp), "Shutdown via dispatch alone is a no-op Ok, got {resp:?}");
    Ok(())
}

// ===========================================================================
// transport: serve / request / EventStream / Shutdown
// ===========================================================================

/// Poll for the socket file to appear, up to ~2s. Bails if it never does so a
/// hung server surfaces as a clear failure instead of a deadlock.
async fn wait_for_socket(path: &Path) -> anyhow::Result<()> {
    for _ in 0..200 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    anyhow::bail!("socket {} never appeared", path.display())
}

#[tokio::test]
async fn given_running_server_when_request_ping_then_ok() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let tmp = tempfile::tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_socket(&sock).await?;

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
    let tmp = tempfile::tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_socket(&sock).await?;

    let resp = ipc::request(
        &sock,
        &Command::AddProject {
            name: "alpha".to_string(),
            repo_path: "/r".to_string(),
            default_branch: "main".to_string(),
            main_agent_cmd: "m".to_string(),
            plan_agent_cmd: "p".to_string(),
            work_agent_cmd: "w".to_string(),
            review_agent_cmd: None,
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
    let tmp = tempfile::tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_socket(&sock).await?;

    let added = ipc::request(
        &sock,
        &Command::AddProject {
            name: "alpha".to_string(),
            repo_path: "/r".to_string(),
            default_branch: "main".to_string(),
            main_agent_cmd: "m".to_string(),
            plan_agent_cmd: "p".to_string(),
            work_agent_cmd: "w".to_string(),
            review_agent_cmd: None,
        },
    )
    .await?;
    assert!(want_id(added) > 0);

    let resp = ipc::request(&sock, &Command::ListProjects).await?;
    assert_eq!(want_projects(resp).len(), 1, "one seeded project must round-trip as a one-element Vec");

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
    let tmp = tempfile::tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_socket(&sock).await?;

    let resp = ipc::request(&sock, &Command::GetProject { project_id: 999_999 }).await?;
    match resp {
        Response::Project(p) => assert_eq!(p, None, "missing project must round-trip as Project(None)"),
        other => panic!("expected Project, got {other:?}"),
    }

    shutdown.notify_one();
    server.await??;
    Ok(())
}

/// Empty Vec newtype variant (`Issues(Vec<_>)`) over the wire: a project with no
/// issues must round-trip ListIssues as an empty Vec.
#[tokio::test]
async fn given_running_server_when_request_list_issues_empty_then_empty_vec() -> anyhow::Result<()> {
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = backlog_seed_project(&db).await?; // seed off-socket; subject is the Issues round-trip.

    let tmp = tempfile::tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_socket(&sock).await?;

    let resp = ipc::request(&sock, &Command::ListIssues { project_id: pid, status: None }).await?;
    match resp {
        Response::Issues(v) => assert!(v.is_empty(), "no issues must round-trip as an empty Vec"),
        other => panic!("expected Issues, got {other:?}"),
    }

    shutdown.notify_one();
    server.await??;
    Ok(())
}

#[tokio::test]
async fn given_running_server_when_shutdown_request_then_server_task_completes() -> anyhow::Result<()>
{
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let tmp = tempfile::tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_socket(&sock).await?;

    let resp = ipc::request(&sock, &Command::Shutdown).await?;
    assert!(is_ok(&resp), "Shutdown request must return Ok, got {resp:?}");
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
    let tmp = tempfile::tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_socket(&sock).await?;

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
    // This test's subject is the EventStream subscription, not Id serialization.
    // Both the project and a pending backlog item are seeded via direct CRUD on
    // the shared pool so the test does not depend on the broken `Response::Id`
    // socket path (see given_running_server_when_request_add_project_then_id).
    // The emitting command is ApproveBacklog: it returns Response::Ok (which
    // serializes cleanly) and broadcasts Event::BacklogChanged.
    let db = Db::open_memory().await?;
    let bus = events::channel();
    let pid = backlog_seed_project(&db).await?;
    let item_id = backlog::add(db.pool(), pid, "x", Source::Agent, None, TS).await?; // pending

    let tmp = tempfile::tempdir()?;
    let sock = tmp.path().join("auwsx.sock");
    let shutdown = Arc::new(Notify::new());
    let server = tokio::spawn({
        let db = db.clone();
        let bus = bus.clone();
        let sock = sock.clone();
        let sd = shutdown.clone();
        async move { ipc::serve(db, bus, &sock, sd).await }
    });
    wait_for_socket(&sock).await?;

    // Open the subscription BEFORE the emitting command.
    let mut stream = ipc::EventStream::connect(&sock).await?;
    let resp = ipc::request(&sock, &Command::ApproveBacklog { item_id }).await?;
    assert!(is_ok(&resp), "ApproveBacklog over socket must return Ok, got {resp:?}");

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

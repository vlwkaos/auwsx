//! Unix-socket Command/Response/Event protocol. Design: revised issue model.
//!
//! Socket path: `$AUWSX_SOCK`, else `$XDG_RUNTIME_DIR/auwsx.sock`, else the
//! platform cache dir, else `$TMPDIR/auwsx.sock`. Wire format is JSON-lines: one
//! [`Command`] or [`Response`] per `\n`-terminated line.
//!
//! The daemon owns the only SQLite write path. Both front-ends (TUI/web) and the
//! agent control CLI are clients; they issue the SAME [`Command`] set. (Agent
//! scoping — restricting a spawned agent to its own issue — is layered on top by
//! the runner via a per-run token; this module carries every op.)
//!
//! [`dispatch`] is the pure request handler (DB + event bus in, [`Response`]
//! out) and is unit-tested directly without a socket; [`serve`] / [`request`]
//! are the thin transport around it.

use crate::artifacts;
use crate::backlog::{self, Approval, BacklogItem, Source};
use crate::db::{
    agent_runs::{self, AgentRun},
    findings::{self, Finding, NewFinding, Severity},
    issues::{self, Issue},
    projects::{
        self, CompletionPolicy, MergeMode, NewProject, Project, UpdateProject as ProjectUpdate,
    },
    scheduler_runs::{self, SchedulerRun},
    subtasks::{self, Subtask},
    Db,
};
use crate::events::Event;
use crate::main_jobs::{self, MainJob};
use crate::routines::{self, Routine, RoutineType};
use crate::scheduler::Scheduler;
use crate::state::IssueStatus;
use crate::steering::{self, Steering, SteeringSource};
use crate::Result;
use anyhow::Context;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Notify};

/// One request from a client to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Ping,

    // --- projects ---
    ListProjects,
    GetProject {
        project_id: i64,
    },
    AddProject {
        name: String,
        repo_path: String,
        default_branch: String,
        main_agent_cmd: String,
        plan_agent_cmd: String,
        work_agent_cmd: String,
        review_agent_cmd: Option<String>,
        /// Policy overrides; `None` leaves the DB DEFAULT untouched.
        completion_policy: Option<CompletionPolicy>,
        plan_gate_timeout_min: Option<i64>,
        completion_soft_timeout_min: Option<i64>,
    },
    UpdateProject {
        project_id: i64,
        name: String,
        repo_path: String,
        default_branch: String,
        main_agent_cmd: String,
        plan_agent_cmd: String,
        work_agent_cmd: String,
        review_agent_cmd: Option<String>,
        completion_policy: CompletionPolicy,
        plan_gate_timeout_min: i64,
        completion_soft_timeout_min: i64,
        iteration_timeout_min: i64,
        main_job_timeout_min: i64,
        review_max_rounds: i64,
        conflict_max_attempts: i64,
        max_concurrency: i64,
        schedule_interval_min: Option<i64>,
        merge_mode: MergeMode,
        skill_path: Option<String>,
        deepsleep_interval_days: i64,
    },

    // --- backlog ---
    ListBacklog {
        project_id: i64,
        approval: Option<Approval>,
    },
    AddBacklog {
        project_id: i64,
        text: String,
        source: Source,
    },
    ApproveBacklog {
        item_id: i64,
    },
    DismissBacklog {
        item_id: i64,
    },
    /// Promote approved, ungrouped backlog items into issues (one issue each).
    Triage {
        project_id: i64,
    },
    UpdateBacklogText {
        item_id: i64,
        text: String,
    },

    // --- issues ---
    ListIssues {
        project_id: i64,
        status: Option<IssueStatus>,
    },
    GetIssue {
        issue_id: i64,
    },
    AddIssue {
        project_id: i64,
        title: String,
        description: Option<String>,
    },
    SetIssueStatus {
        issue_id: i64,
        status: IssueStatus,
        /// Human override: skip the legal-transition check.
        force: bool,
    },
    AbsorbIssue {
        issue_id: i64,
        into_issue_id: i64,
    },
    RunSchedulerOnce {
        project_id: i64,
    },
    RunIssueNow {
        issue_id: i64,
    },
    RunBacklogNow {
        item_id: i64,
    },
    RunRoutineNow {
        routine_id: i64,
    },

    // --- routines / activity ---
    ListRoutines {
        project_id: i64,
    },
    GetRoutine {
        routine_id: i64,
    },
    ToggleRoutine {
        routine_id: i64,
        enabled: bool,
    },
    CreateRoutine {
        project_id: i64,
        name: String,
        routine_type: RoutineType,
        prompt: String,
        cron: String,
        writable_paths: Option<String>,
        enabled: bool,
    },
    UpdateRoutine {
        routine_id: i64,
        name: String,
        routine_type: RoutineType,
        prompt: String,
        cron: String,
        writable_paths: Option<String>,
        enabled: bool,
    },
    RecentAgentRunsByProject {
        project_id: i64,
        limit: i64,
    },
    ListAgentRunsByIssue {
        issue_id: i64,
    },
    RecentMainJobsByProject {
        project_id: i64,
        limit: i64,
    },
    RecentMainJobsByRoutine {
        routine_id: i64,
        limit: i64,
    },
    RecentSchedulerRunsByProject {
        project_id: i64,
        limit: i64,
    },
    TailAgentRunLog {
        agent_run_id: i64,
        max_bytes: usize,
    },

    // --- subtasks ---
    ListSubtasks {
        issue_id: i64,
    },
    AddSubtask {
        issue_id: i64,
        ord: i64,
        text: String,
    },
    CompleteSubtask {
        subtask_id: i64,
    },

    // --- findings ---
    ListFindings {
        issue_id: i64,
        open_only: bool,
    },
    AddFinding {
        issue_id: i64,
        review_round: i64,
        severity: Severity,
        lens: Option<String>,
        title: String,
        detail: Option<String>,
        file_ref: Option<String>,
    },
    AcceptFinding {
        finding_id: i64,
        rationale: String,
    },
    RejectFinding {
        finding_id: i64,
        rationale: String,
    },
    DismissFinding {
        finding_id: i64,
    },

    // --- steering ---
    ListSteering {
        issue_id: i64,
        pending_only: bool,
    },
    AddSteering {
        issue_id: i64,
        source: SteeringSource,
        note: String,
    },
    ConsumeSteering {
        issue_id: i64,
    },

    // --- lifecycle ---
    /// Open an event subscription on this connection (server streams
    /// `Response::Event` lines until the client disconnects).
    Subscribe,
    /// Ask the daemon to shut down gracefully.
    Shutdown,
}

/// One reply from the daemon. Request/response is one [`Response`] per
/// [`Command`]; a `Subscribe` connection then streams `Event` variants.
///
/// Adjacent tagging (`kind` + `data`) is required, not internal tagging: several
/// variants wrap a primitive (`Id(i64)`), a sequence (`Projects(Vec<_>)`), or an
/// option (`Project(Option<_>)`), none of which serde_json can serialize under
/// an internal `tag`. Adjacent tagging puts the payload in its own `data` field,
/// so every variant shape round-trips.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Err { message: String },
    Id(i64),
    Projects(Vec<Project>),
    Project(Option<Project>),
    Backlog(Vec<BacklogItem>),
    Issues(Vec<Issue>),
    Issue(Option<Issue>),
    Subtasks(Vec<Subtask>),
    Findings(Vec<Finding>),
    Steering(Vec<Steering>),
    Routines(Vec<Routine>),
    Routine(Option<Routine>),
    AgentRuns(Vec<AgentRun>),
    MainJobs(Vec<MainJob>),
    SchedulerRuns(Vec<SchedulerRun>),
    LogTail { path: String, text: String },
    Triaged { created_issue_ids: Vec<i64> },
    RanIssue { issue_id: i64 },
    Event(Event),
}

impl Response {
    fn err(e: impl std::fmt::Display) -> Self {
        Response::Err {
            message: e.to_string(),
        }
    }
}

/// Resolve the socket path: `$AUWSX_SOCK` override, then `$XDG_RUNTIME_DIR`,
/// then the platform cache dir, then `$TMPDIR`.
pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("AUWSX_SOCK") {
        return PathBuf::from(p);
    }
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        if !rt.is_empty() {
            return PathBuf::from(rt).join("auwsx.sock");
        }
    }
    if let Some(dirs) = ProjectDirs::from("", "", "auwsx") {
        return dirs.cache_dir().join("auwsx.sock");
    }
    std::env::temp_dir().join("auwsx.sock")
}

/// Daemon wall clock (epoch ms). The daemon is the single clock owner; the CRUD
/// layer takes `now` explicitly so it stays deterministic under test.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Handle one command against the database, emitting events for state changes.
/// `now` is the timestamp to stamp writes with (injected for testability).
///
/// `Subscribe`/`Shutdown` are lifecycle signals handled by the transport layer;
/// here they just return `Ok`.
pub async fn dispatch(
    db: &Db,
    events: &broadcast::Sender<Event>,
    now: i64,
    cmd: Command,
) -> Response {
    match dispatch_inner(db, events, now, cmd).await {
        Ok(resp) => resp,
        Err(e) => Response::err(e),
    }
}

async fn dispatch_inner(
    db: &Db,
    events: &broadcast::Sender<Event>,
    now: i64,
    cmd: Command,
) -> Result<Response> {
    let pool = db.pool();
    Ok(match cmd {
        Command::Ping | Command::Subscribe | Command::Shutdown => Response::Ok,

        // --- projects ---
        Command::ListProjects => Response::Projects(projects::list(pool).await?),
        Command::GetProject { project_id } => {
            Response::Project(projects::get(pool, project_id).await?)
        }
        Command::AddProject {
            name,
            repo_path,
            default_branch,
            main_agent_cmd,
            plan_agent_cmd,
            work_agent_cmd,
            review_agent_cmd,
            completion_policy,
            plan_gate_timeout_min,
            completion_soft_timeout_min,
        } => {
            let id = projects::create(
                pool,
                NewProject {
                    name: &name,
                    repo_path: &repo_path,
                    default_branch: &default_branch,
                    main_agent_cmd: &main_agent_cmd,
                    plan_agent_cmd: &plan_agent_cmd,
                    work_agent_cmd: &work_agent_cmd,
                    review_agent_cmd: review_agent_cmd.as_deref(),
                    completion_policy,
                    plan_gate_timeout_min,
                    completion_soft_timeout_min,
                },
                now,
            )
            .await?;
            Response::Id(id)
        }
        Command::UpdateProject {
            project_id,
            name,
            repo_path,
            default_branch,
            main_agent_cmd,
            plan_agent_cmd,
            work_agent_cmd,
            review_agent_cmd,
            completion_policy,
            plan_gate_timeout_min,
            completion_soft_timeout_min,
            iteration_timeout_min,
            main_job_timeout_min,
            review_max_rounds,
            conflict_max_attempts,
            max_concurrency,
            schedule_interval_min,
            merge_mode,
            skill_path,
            deepsleep_interval_days,
        } => {
            projects::update(
                pool,
                project_id,
                ProjectUpdate {
                    name: &name,
                    repo_path: &repo_path,
                    default_branch: &default_branch,
                    main_agent_cmd: &main_agent_cmd,
                    plan_agent_cmd: &plan_agent_cmd,
                    work_agent_cmd: &work_agent_cmd,
                    review_agent_cmd: review_agent_cmd.as_deref(),
                    completion_policy,
                    plan_gate_timeout_min,
                    completion_soft_timeout_min,
                    iteration_timeout_min,
                    main_job_timeout_min,
                    review_max_rounds,
                    conflict_max_attempts,
                    max_concurrency,
                    schedule_interval_min,
                    merge_mode,
                    skill_path: skill_path.as_deref(),
                    deepsleep_interval_days,
                },
            )
            .await?;
            Response::Ok
        }

        // --- backlog ---
        Command::ListBacklog {
            project_id,
            approval,
        } => {
            let items = match approval {
                Some(a) => backlog::list_by_approval(pool, project_id, a).await?,
                None => backlog::list_by_project(pool, project_id).await?,
            };
            Response::Backlog(items)
        }
        Command::AddBacklog {
            project_id,
            text,
            source,
        } => {
            let id = backlog::add(pool, project_id, &text, source, None, now).await?;
            emit(
                events,
                Event::BacklogChanged {
                    item_id: id,
                    project_id,
                    approval: source.default_approval().as_str().to_string(),
                },
            );
            Response::Id(id)
        }
        Command::ApproveBacklog { item_id } => {
            backlog::approve(pool, item_id, now).await?;
            emit_backlog_changed(events, pool, item_id, "approved").await;
            Response::Ok
        }
        Command::DismissBacklog { item_id } => {
            backlog::dismiss(pool, item_id, now).await?;
            emit_backlog_changed(events, pool, item_id, "dismissed").await;
            Response::Ok
        }
        Command::Triage { project_id } => {
            let created = backlog::run_triage(pool, project_id, now).await?;
            Response::Triaged {
                created_issue_ids: created,
            }
        }
        Command::UpdateBacklogText { item_id, text } => {
            let item = backlog::get(pool, item_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("backlog item {item_id} not found"))?;
            if item.consumed_issue_id.is_some() {
                anyhow::bail!("backlog item {item_id} is already consumed");
            }
            backlog::edit_text(pool, item_id, &text).await?;
            emit_backlog_changed(events, pool, item_id, item.approval.as_str()).await;
            Response::Ok
        }

        // --- issues ---
        Command::ListIssues { project_id, status } => {
            let issues = match status {
                Some(s) => issues::list_by_status(pool, project_id, s).await?,
                None => issues::list_by_project(pool, project_id).await?,
            };
            Response::Issues(issues)
        }
        Command::GetIssue { issue_id } => Response::Issue(issues::get(pool, issue_id).await?),
        Command::AddIssue {
            project_id,
            title,
            description,
        } => {
            let id = issues::create(pool, project_id, &title, description.as_deref(), now).await?;
            Response::Id(id)
        }
        Command::SetIssueStatus {
            issue_id,
            status,
            force,
        } => {
            if status == IssueStatus::Absorbed && !force {
                anyhow::bail!("use `issue absorb <issue_id> <into_issue_id>` to record the target");
            }
            if force {
                issues::force_status(pool, issue_id, status, now).await?;
            } else {
                issues::transition(pool, issue_id, status, now).await?;
            }
            emit(events, Event::IssueStatus { issue_id, status });
            Response::Ok
        }
        Command::AbsorbIssue {
            issue_id,
            into_issue_id,
        } => {
            issues::mark_absorbed(pool, issue_id, into_issue_id, now).await?;
            emit(
                events,
                Event::IssueStatus {
                    issue_id,
                    status: IssueStatus::Absorbed,
                },
            );
            Response::Ok
        }
        Command::RunSchedulerOnce { .. }
        | Command::RunIssueNow { .. }
        | Command::RunBacklogNow { .. }
        | Command::RunRoutineNow { .. } => {
            anyhow::bail!("manual run commands require the daemon runtime")
        }

        // --- routines / activity ---
        Command::ListRoutines { project_id } => {
            Response::Routines(routines::list_by_project(pool, project_id).await?)
        }
        Command::GetRoutine { routine_id } => {
            Response::Routine(routines::get(pool, routine_id).await?)
        }
        Command::ToggleRoutine {
            routine_id,
            enabled,
        } => {
            routines::set_enabled(pool, routine_id, enabled).await?;
            Response::Ok
        }
        Command::CreateRoutine {
            project_id,
            name,
            routine_type,
            prompt,
            cron,
            writable_paths,
            enabled,
        } => Response::Id(
            routines::create(
                pool,
                routines::NewRoutine {
                    project_id,
                    name: &name,
                    routine_type,
                    prompt: &prompt,
                    cron: &cron,
                    writable_paths: writable_paths.as_deref(),
                    enabled,
                },
                now,
            )
            .await?,
        ),
        Command::UpdateRoutine {
            routine_id,
            name,
            routine_type,
            prompt,
            cron,
            writable_paths,
            enabled,
        } => {
            routines::update(
                pool,
                routine_id,
                routines::UpdateRoutine {
                    name: &name,
                    routine_type,
                    prompt: &prompt,
                    cron: &cron,
                    writable_paths: writable_paths.as_deref(),
                    enabled,
                },
            )
            .await?;
            Response::Ok
        }
        Command::RecentAgentRunsByProject { project_id, limit } => {
            Response::AgentRuns(agent_runs::recent_by_project(pool, project_id, limit).await?)
        }
        Command::ListAgentRunsByIssue { issue_id } => {
            Response::AgentRuns(agent_runs::list_by_issue(pool, issue_id).await?)
        }
        Command::RecentMainJobsByProject { project_id, limit } => {
            Response::MainJobs(main_jobs::recent_by_project(pool, project_id, limit).await?)
        }
        Command::RecentMainJobsByRoutine { routine_id, limit } => {
            Response::MainJobs(main_jobs::recent_by_routine(pool, routine_id, limit).await?)
        }
        Command::RecentSchedulerRunsByProject { project_id, limit } => Response::SchedulerRuns(
            scheduler_runs::recent_by_project(pool, project_id, limit).await?,
        ),
        Command::TailAgentRunLog {
            agent_run_id,
            max_bytes,
        } => {
            let run = agent_runs::get(pool, agent_run_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("agent_run {agent_run_id} not found"))?;
            let path = run
                .log_path
                .ok_or_else(|| anyhow::anyhow!("agent_run {agent_run_id} has no log_path"))?;
            Response::LogTail {
                text: artifacts::tail_file(PathBuf::from(&path), max_bytes).await?,
                path,
            }
        }

        // --- subtasks ---
        Command::ListSubtasks { issue_id } => {
            Response::Subtasks(subtasks::list_by_issue(pool, issue_id).await?)
        }
        Command::AddSubtask {
            issue_id,
            ord,
            text,
        } => Response::Id(subtasks::add(pool, issue_id, ord, &text, now).await?),
        Command::CompleteSubtask { subtask_id } => {
            subtasks::mark_done(pool, subtask_id, now).await?;
            Response::Ok
        }

        // --- findings ---
        Command::ListFindings {
            issue_id,
            open_only,
        } => {
            let f = if open_only {
                findings::list_open(pool, issue_id).await?
            } else {
                findings::list_by_issue(pool, issue_id).await?
            };
            Response::Findings(f)
        }
        Command::AddFinding {
            issue_id,
            review_round,
            severity,
            lens,
            title,
            detail,
            file_ref,
        } => {
            let id = findings::add(
                pool,
                NewFinding {
                    issue_id,
                    review_round,
                    severity,
                    lens: lens.as_deref(),
                    title: &title,
                    detail: detail.as_deref(),
                    file_ref: file_ref.as_deref(),
                },
                now,
            )
            .await?;
            emit(
                events,
                Event::FindingAdded {
                    finding_id: id,
                    issue_id,
                },
            );
            Response::Id(id)
        }
        Command::AcceptFinding {
            finding_id,
            rationale,
        } => {
            findings::accept(pool, finding_id, &rationale, now).await?;
            Response::Ok
        }
        Command::RejectFinding {
            finding_id,
            rationale,
        } => {
            findings::reject(pool, finding_id, &rationale, now).await?;
            Response::Ok
        }
        Command::DismissFinding { finding_id } => {
            findings::dismiss(pool, finding_id, now).await?;
            Response::Ok
        }

        // --- steering ---
        Command::ListSteering {
            issue_id,
            pending_only: _,
        } => {
            // Only pending steering has a CRUD reader today (it is the meaningful
            // set — consumed notes are history); `pending_only` is reserved for a
            // future "all steering" view.
            Response::Steering(steering::list_pending(pool, issue_id).await?)
        }
        Command::AddSteering {
            issue_id,
            source,
            note,
        } => {
            let id = steering::add(pool, issue_id, source, &note, now).await?;
            emit(
                events,
                Event::SteeringAdded {
                    steering_id: id,
                    issue_id,
                },
            );
            Response::Id(id)
        }
        Command::ConsumeSteering { issue_id } => {
            steering::consume_all(pool, issue_id, now).await?;
            Response::Ok
        }
    })
}

/// Send an event, ignoring "no subscribers" (events are advisory).
fn emit(events: &broadcast::Sender<Event>, ev: Event) {
    let _ = events.send(ev);
}

/// Look up a backlog item's project to emit a `BacklogChanged` after a state
/// change. Best-effort: a lookup miss just skips the event.
async fn emit_backlog_changed(
    events: &broadcast::Sender<Event>,
    pool: &sqlx::SqlitePool,
    item_id: i64,
    approval: &str,
) {
    if let Ok(Some(item)) = backlog::get(pool, item_id).await {
        emit(
            events,
            Event::BacklogChanged {
                item_id,
                project_id: item.project_id,
                approval: approval.to_string(),
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Run the IPC server until `shutdown` is notified (a `Shutdown` command also
/// triggers it). Binds `socket`, removing a stale file first; removes the file
/// on exit. Each connection is served on its own task.
pub async fn serve(
    db: Db,
    events: broadcast::Sender<Event>,
    socket: &Path,
    shutdown: Arc<Notify>,
) -> Result<()> {
    serve_inner(db, events, socket, shutdown, None).await
}

pub async fn serve_with_scheduler(
    db: Db,
    events: broadcast::Sender<Event>,
    socket: &Path,
    shutdown: Arc<Notify>,
    scheduler: Arc<Scheduler>,
) -> Result<()> {
    serve_inner(db, events, socket, shutdown, Some(scheduler)).await
}

async fn serve_inner(
    db: Db,
    events: broadcast::Sender<Event>,
    socket: &Path,
    shutdown: Arc<Notify>,
    scheduler: Option<Arc<Scheduler>>,
) -> Result<()> {
    let bind_socket = if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {}", parent.display()))?;
        match std::fs::canonicalize(parent) {
            Ok(parent) => parent.join(socket.file_name().unwrap_or_default()),
            Err(_) => socket.to_path_buf(),
        }
    } else {
        socket.to_path_buf()
    };
    // A leftover socket from a crashed daemon would make bind() fail with
    // EADDRINUSE even though nobody is listening; clear it first.
    if bind_socket.exists() {
        std::fs::remove_file(&bind_socket).ok();
    }
    let listener = UnixListener::bind(&bind_socket)
        .with_context(|| format!("binding unix socket {}", bind_socket.display()))?;

    loop {
        tokio::select! {
            _ = shutdown.notified() => break,
            accepted = listener.accept() => {
                let (stream, _addr) = accepted.context("accepting connection")?;
                let db = db.clone();
                let events = events.clone();
                let shutdown = shutdown.clone();
                let scheduler = scheduler.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, db, events, shutdown, scheduler).await {
                        tracing::debug!("ipc connection ended: {e:#}");
                    }
                });
            }
        }
    }

    std::fs::remove_file(&bind_socket).ok();
    Ok(())
}

async fn handle_conn(
    stream: UnixStream,
    db: Db,
    events: broadcast::Sender<Event>,
    shutdown: Arc<Notify>,
    scheduler: Option<Arc<Scheduler>>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let cmd: Command = match serde_json::from_str(&line) {
            Ok(c) => c,
            Err(e) => {
                write_line(&mut write_half, &Response::err(format!("bad command: {e}"))).await?;
                continue;
            }
        };

        match cmd {
            Command::Subscribe => {
                write_line(&mut write_half, &Response::Ok).await?;
                stream_events(&mut write_half, events.subscribe()).await?;
                return Ok(()); // subscription owns the connection for its lifetime
            }
            Command::Shutdown => {
                write_line(&mut write_half, &Response::Ok).await?;
                shutdown.notify_one();
                return Ok(());
            }
            manual @ (Command::RunSchedulerOnce { .. }
            | Command::RunIssueNow { .. }
            | Command::RunBacklogNow { .. }
            | Command::RunRoutineNow { .. }) => {
                let resp = dispatch_manual(&db, scheduler.as_deref(), now_ms(), manual).await;
                write_line(&mut write_half, &resp).await?;
            }
            other => {
                let resp = dispatch(&db, &events, now_ms(), other).await;
                write_line(&mut write_half, &resp).await?;
            }
        }
    }
    Ok(())
}

async fn dispatch_manual(
    db: &Db,
    scheduler: Option<&Scheduler>,
    now: i64,
    cmd: Command,
) -> Response {
    let Some(scheduler) = scheduler else {
        return Response::Err {
            message: "manual run commands require the daemon runtime".to_string(),
        };
    };
    match manual_inner(db, scheduler, now, cmd).await {
        Ok(resp) => resp,
        Err(e) => Response::err(e),
    }
}

async fn manual_inner(_db: &Db, scheduler: &Scheduler, now: i64, cmd: Command) -> Result<Response> {
    Ok(match cmd {
        Command::RunSchedulerOnce { project_id } => {
            scheduler.tick_project(project_id).await?;
            Response::Ok
        }
        Command::RunIssueNow { issue_id } => {
            scheduler.run_issue_now(issue_id).await?;
            Response::RanIssue { issue_id }
        }
        Command::RunBacklogNow { item_id } => {
            let issue_id = scheduler.run_backlog_now(item_id, now).await?;
            Response::RanIssue { issue_id }
        }
        Command::RunRoutineNow { routine_id } => {
            let _main_job_id = scheduler.run_routine_now(routine_id).await?;
            Response::Ok
        }
        _ => anyhow::bail!("not a manual command"),
    })
}

/// Forward broadcast events to a subscribed client until it disconnects or the
/// channel closes. A lagging client (buffer overrun) is skipped forward, not
/// dropped — it should resync from the DB.
async fn stream_events<W>(write: &mut W, mut rx: broadcast::Receiver<Event>) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if write_line(write, &Response::Event(ev)).await.is_err() {
                    break; // client gone
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

async fn write_line<W>(write: &mut W, resp: &Response) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let mut buf = serde_json::to_vec(resp)?;
    buf.push(b'\n');
    write.write_all(&buf).await?;
    write.flush().await?;
    Ok(())
}

/// One-shot request/response: connect, send `cmd`, read one `Response`.
pub async fn request(socket: &Path, cmd: &Command) -> Result<Response> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to daemon at {}", socket.display()))?;
    let mut buf = serde_json::to_vec(cmd)?;
    buf.push(b'\n');
    stream.write_all(&buf).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        anyhow::bail!("daemon closed connection without responding");
    }
    Ok(serde_json::from_str(&line)?)
}

/// A live event subscription. Each [`next`](EventStream::next) yields one
/// daemon [`Event`]; `None` means the daemon closed the connection.
pub struct EventStream {
    lines: tokio::io::Lines<BufReader<UnixStream>>,
}

impl EventStream {
    /// Connect and open a subscription. Consumes the leading `Response::Ok`
    /// acknowledgement so the first [`next`](EventStream::next) yields an event.
    pub async fn connect(socket: &Path) -> Result<Self> {
        let mut stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to daemon at {}", socket.display()))?;
        let mut buf = serde_json::to_vec(&Command::Subscribe)?;
        buf.push(b'\n');
        stream.write_all(&buf).await?;
        stream.flush().await?;

        let mut lines = BufReader::new(stream).lines();
        // Drain the Ok ack.
        match lines.next_line().await? {
            Some(_) => {}
            None => anyhow::bail!("daemon closed subscription immediately"),
        }
        Ok(EventStream { lines })
    }

    /// Next event, or `None` when the daemon disconnects.
    pub async fn next(&mut self) -> Result<Option<Event>> {
        let Some(line) = self.lines.next_line().await? else {
            return Ok(None);
        };
        match serde_json::from_str::<Response>(&line)? {
            Response::Event(ev) => Ok(Some(ev)),
            other => anyhow::bail!("expected event, got {other:?}"),
        }
    }
}

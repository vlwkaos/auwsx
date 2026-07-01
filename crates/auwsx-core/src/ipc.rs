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
    arsenal::{self, ArsenalPreset, NewArsenalPreset},
    ask_answers::{self, AskAnswer, AskMode},
    findings::{self, Finding, NewFinding, Severity},
    global_settings::{self, GlobalSettings},
    issues::{self, Issue},
    memory_presets::{self, MemoryPreset, NewMemoryPreset},
    profiles::{self, Profile},
    projects::{
        self, CompletionPolicy, MergeMode, NewProject, Project, UpdateProject as ProjectUpdate,
    },
    remote::{
        self, ProjectRemoteConfig, RemoteAuthKind, RemoteIssueLink, RemotePrLink, RemoteProvider,
        RemoteSyncRun, RequiredChecksPolicy, UpsertProjectRemoteConfig,
    },
    scheduler_runs::{self, SchedulerRun},
    subtasks::{self, Subtask},
    Db,
};
use crate::events::Event;
use crate::issue_control::ControlOutcome;
use crate::main_jobs::{self, MainJob};
use crate::memory;
use crate::project_setup;
use crate::reconcile::ProjectReconcileReport;
use crate::remote_inbound::{self, ProcessRemoteAuwsxRunInput, RemoteInboundOutcome};
use crate::remote_plan::{self, RemoteWorkflowInput, RemoteWorkflowPlan};
use crate::routines::{self, OutputRoute, Routine};
use crate::routing;
use crate::scheduler::Scheduler;
use crate::state::IssueStatus;
use crate::steering::{self, Steering, SteeringSource};
use crate::Result;
use crate::{local_merge, local_merge::LocalMergeOutcome};
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

    // --- global config ---
    ListArsenalPresets,
    ListMemoryPresets,
    GetGlobalSettings,
    UpdateGlobalSettings {
        memory_preset_name: String,
        pipeline_ux_guidance: String,
    },
    UpsertArsenalPreset {
        name: String,
        main_agent_cmd: String,
        #[serde(default)]
        route_agent_cmd: String,
        plan_agent_cmd: String,
        work_agent_cmd: String,
        review_agent_cmd: Option<String>,
    },
    UpsertMemoryPreset {
        name: String,
        retrieve_kind: String,
        retrieve_cmd: Option<String>,
        save_kind: String,
        save_cmd: Option<String>,
        dream_kind: String,
        dream_cmd: Option<String>,
        deepsleep_kind: String,
        deepsleep_cmd: Option<String>,
    },
    ListAskAnswers {
        project_id: i64,
        limit: i64,
    },
    MemoryRetrieve {
        project_id: i64,
        query: String,
    },
    MemorySave {
        project_id: i64,
        kind: String,
        content: String,
    },
    MemoryConsolidate {
        project_id: i64,
        mode: String,
    },
    AskProject {
        project_id: i64,
        mode: AskMode,
        question: String,
    },
    ListProfiles,
    CreateProfile {
        name: String,
    },
    RenameProfile {
        profile_id: i64,
        name: String,
    },
    MoveProjectToProfile {
        project_id: i64,
        profile_id: i64,
    },
    MoveProjectInProfile {
        project_id: i64,
        delta: isize,
    },

    // --- projects ---
    ListProjects,
    GetProject {
        project_id: i64,
    },
    AddProject {
        name: String,
        repo_path: String,
        default_branch: String,
        arsenal_preset_name: Option<String>,
        main_agent_cmd: String,
        #[serde(default)]
        route_agent_cmd: String,
        plan_agent_cmd: String,
        work_agent_cmd: String,
        review_agent_cmd: Option<String>,
        /// Policy overrides; `None` leaves the DB DEFAULT untouched.
        completion_policy: Option<CompletionPolicy>,
        plan_gate_timeout_min: Option<i64>,
        completion_soft_timeout_min: Option<i64>,
        /// User-facing autonomous cadence: 5-field cron, @tick, @every, or normalized shorthand.
        schedule_cron: Option<String>,
    },
    UpdateProject {
        project_id: i64,
        name: String,
        repo_path: String,
        default_branch: String,
        arsenal_preset_name: Option<String>,
        main_agent_cmd: String,
        #[serde(default)]
        route_agent_cmd: String,
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
        schedule_cron: Option<String>,
        merge_mode: MergeMode,
        skill_path: Option<String>,
        deepsleep_cron: Option<String>,
    },
    RemoveProject {
        project_id: i64,
        /// Delete only DB registration/history, leaving any live agent process
        /// or worktree cleanup to the operator.
        shallow: bool,
    },
    DiagnoseProject {
        project_id: i64,
    },
    ReconcileProject {
        project_id: i64,
        dry_run: bool,
    },
    ApplyReconcile {
        main_job_id: i64,
    },
    GetProjectRemoteConfig {
        project_id: i64,
    },
    GetProjectRemoteConfigByRepo {
        provider: RemoteProvider,
        owner: String,
        repo: String,
    },
    UpsertProjectRemoteConfig {
        project_id: i64,
        provider: RemoteProvider,
        remote_url: String,
        owner: String,
        repo: String,
        api_base_url: String,
        auth_kind: RemoteAuthKind,
        auth_ref: Option<String>,
        webhook_secret_ref: Option<String>,
        inbound_auwsx_run_enabled: bool,
        outbound_issue_create_enabled: bool,
        remote_pr_merge_enabled: bool,
        agent_comment_sync_enabled: bool,
        subtask_comment_sync_enabled: bool,
        finding_comment_sync_enabled: bool,
        draft_pr_enabled: bool,
        required_checks_policy: RequiredChecksPolicy,
        default_labels: Option<String>,
        default_assignees: Option<String>,
        pr_base_branch: Option<String>,
    },
    DeleteProjectRemoteConfig {
        project_id: i64,
    },
    RecentRemoteSyncRuns {
        project_id: i64,
        limit: i64,
    },
    PlanIssueRemoteWorkflow {
        issue_id: i64,
    },
    GetIssueRemoteLinks {
        issue_id: i64,
    },
    ProcessRemoteAuwsxRun {
        provider: RemoteProvider,
        delivery_id: String,
        event_kind: String,
        action: Option<String>,
        payload_hash: String,
        owner: String,
        repo: String,
        remote_issue_number: i64,
        remote_issue_node_id: Option<String>,
        remote_issue_title: String,
        remote_issue_url: String,
        comment_body: String,
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
    RemoveIssue {
        issue_id: i64,
    },
    AbandonIssue {
        issue_id: i64,
    },
    CleanupIssueWorktree {
        issue_id: i64,
    },
    ApplyIssueMerge {
        issue_id: i64,
    },
    RunSchedulerOnce {
        project_id: i64,
    },
    ExecuteProject {
        project_id: i64,
    },
    ExecuteIssue {
        issue_id: i64,
    },
    RunIssueNow {
        issue_id: i64,
    },
    RetryIssue {
        issue_id: i64,
    },
    ApproveIssueMerge {
        issue_id: i64,
    },
    ApproveProjectMerge {
        project_id: i64,
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
        output_route: OutputRoute,
        prompt: String,
        cron: String,
        writable_paths: Option<String>,
        enabled: bool,
    },
    UpdateRoutine {
        routine_id: i64,
        name: String,
        output_route: OutputRoute,
        prompt: String,
        cron: String,
        writable_paths: Option<String>,
        enabled: bool,
    },
    RemoveRoutine {
        routine_id: i64,
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
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Err { message: String },
    Id(i64),
    AskAnswers(Vec<AskAnswer>),
    ArsenalPresets(Vec<ArsenalPreset>),
    MemoryPresets(Vec<MemoryPreset>),
    GlobalSettings(GlobalSettings),
    Profiles(Vec<Profile>),
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
    MemoryText { text: String },
    Triaged { created_issue_ids: Vec<i64> },
    RanIssue { issue_id: i64 },
    ApprovedMerge { issue_ids: Vec<i64> },
    ReconcileReport(ProjectReconcileReport),
    ProjectRemoteConfig(Option<ProjectRemoteConfig>),
    RemoteSyncRuns(Vec<RemoteSyncRun>),
    IssueRemoteWorkflowPlan(RemoteWorkflowPlan),
    IssueRemoteLinks(IssueRemoteLinks),
    RemoteInboundOutcome(RemoteInboundOutcome),
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueRemoteLinks {
    pub issue_link: Option<RemoteIssueLink>,
    pub pr_link: Option<RemotePrLink>,
}

impl Response {
    fn err(e: impl std::fmt::Display) -> Self {
        Response::Err {
            message: e.to_string(),
        }
    }
}

impl From<ControlOutcome> for Response {
    fn from(outcome: ControlOutcome) -> Self {
        match outcome {
            ControlOutcome::Ok => Response::Ok,
            ControlOutcome::RanIssue { issue_id } => Response::RanIssue { issue_id },
            ControlOutcome::ApprovedMerge { issue_ids } => Response::ApprovedMerge { issue_ids },
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

        // --- global config ---
        Command::ListArsenalPresets => Response::ArsenalPresets(arsenal::list(pool).await?),
        Command::ListMemoryPresets => Response::MemoryPresets(memory_presets::list(pool).await?),
        Command::GetGlobalSettings => Response::GlobalSettings(global_settings::get(pool).await?),
        Command::UpdateGlobalSettings {
            memory_preset_name,
            pipeline_ux_guidance,
        } => {
            global_settings::update(pool, &memory_preset_name, &pipeline_ux_guidance, now).await?;
            Response::Ok
        }
        Command::UpsertArsenalPreset {
            name,
            main_agent_cmd,
            route_agent_cmd,
            plan_agent_cmd,
            work_agent_cmd,
            review_agent_cmd,
        } => {
            let route_agent_cmd = if route_agent_cmd.trim().is_empty() {
                work_agent_cmd.as_str()
            } else {
                route_agent_cmd.as_str()
            };
            Response::Id(
                arsenal::upsert(
                    pool,
                    NewArsenalPreset {
                        name: &name,
                        main_agent_cmd: &main_agent_cmd,
                        route_agent_cmd,
                        plan_agent_cmd: &plan_agent_cmd,
                        work_agent_cmd: &work_agent_cmd,
                        review_agent_cmd: review_agent_cmd.as_deref(),
                    },
                    now,
                )
                .await?,
            )
        }
        Command::UpsertMemoryPreset {
            name,
            retrieve_kind,
            retrieve_cmd,
            save_kind,
            save_cmd,
            dream_kind,
            dream_cmd,
            deepsleep_kind,
            deepsleep_cmd,
        } => Response::Id(
            memory_presets::upsert(
                pool,
                NewMemoryPreset {
                    name: &name,
                    retrieve_kind: &retrieve_kind,
                    retrieve_cmd: retrieve_cmd.as_deref(),
                    save_kind: &save_kind,
                    save_cmd: save_cmd.as_deref(),
                    dream_kind: &dream_kind,
                    dream_cmd: dream_cmd.as_deref(),
                    deepsleep_kind: &deepsleep_kind,
                    deepsleep_cmd: deepsleep_cmd.as_deref(),
                },
                now,
            )
            .await?,
        ),
        Command::ListAskAnswers { project_id, limit } => {
            Response::AskAnswers(ask_answers::list_by_project(pool, project_id, limit).await?)
        }
        Command::MemoryRetrieve { project_id, query } => Response::MemoryText {
            text: memory::retrieve(db, project_id, &query).await?,
        },
        Command::MemorySave {
            project_id,
            kind,
            content,
        } => Response::MemoryText {
            text: memory::save(db, project_id, &kind, &content).await?,
        },
        Command::MemoryConsolidate { project_id, mode } => Response::MemoryText {
            text: memory::consolidate(db, project_id, &mode).await?,
        },
        Command::AskProject { .. } => {
            anyhow::bail!("ask commands require the daemon runtime")
        }
        Command::ListProfiles => Response::Profiles(profiles::list(pool).await?),
        Command::CreateProfile { name } => Response::Id(profiles::create(pool, &name, now).await?),
        Command::RenameProfile { profile_id, name } => {
            profiles::rename(pool, profile_id, &name).await?;
            Response::Ok
        }
        Command::MoveProjectToProfile {
            project_id,
            profile_id,
        } => {
            projects::move_to_profile(pool, project_id, profile_id).await?;
            Response::Ok
        }
        Command::MoveProjectInProfile { project_id, delta } => {
            projects::move_within_profile(pool, project_id, delta).await?;
            Response::Ok
        }

        // --- projects ---
        Command::ListProjects => Response::Projects(projects::list(pool).await?),
        Command::GetProject { project_id } => {
            Response::Project(projects::get(pool, project_id).await?)
        }
        Command::AddProject {
            name,
            repo_path,
            default_branch,
            arsenal_preset_name,
            main_agent_cmd,
            route_agent_cmd,
            plan_agent_cmd,
            work_agent_cmd,
            review_agent_cmd,
            completion_policy,
            plan_gate_timeout_min,
            completion_soft_timeout_min,
            schedule_cron,
        } => {
            let route_agent_cmd =
                if route_agent_cmd.trim().is_empty() && arsenal_preset_name.is_none() {
                    work_agent_cmd.as_str()
                } else {
                    route_agent_cmd.as_str()
                };
            let id = projects::create(
                pool,
                NewProject {
                    name: &name,
                    repo_path: &repo_path,
                    default_branch: &default_branch,
                    arsenal_preset_name: arsenal_preset_name.as_deref(),
                    main_agent_cmd: &main_agent_cmd,
                    route_agent_cmd,
                    plan_agent_cmd: &plan_agent_cmd,
                    work_agent_cmd: &work_agent_cmd,
                    review_agent_cmd: review_agent_cmd.as_deref(),
                    completion_policy,
                    plan_gate_timeout_min,
                    completion_soft_timeout_min,
                    schedule_cron: schedule_cron.as_deref(),
                },
                now,
            )
            .await?;
            if let Err(e) =
                project_setup::ensure_agents_knowledge_block(Path::new(&repo_path), &name)
            {
                tracing::warn!("setting up AGENTS.md for project {name:?} failed: {e:#}");
            }
            Response::Id(id)
        }
        Command::UpdateProject {
            project_id,
            name,
            repo_path,
            default_branch,
            arsenal_preset_name,
            main_agent_cmd,
            route_agent_cmd,
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
            schedule_cron,
            merge_mode,
            skill_path,
            deepsleep_cron,
        } => {
            let route_agent_cmd =
                if route_agent_cmd.trim().is_empty() && arsenal_preset_name.is_none() {
                    work_agent_cmd.as_str()
                } else {
                    route_agent_cmd.as_str()
                };
            projects::update(
                pool,
                project_id,
                ProjectUpdate {
                    name: &name,
                    repo_path: &repo_path,
                    default_branch: &default_branch,
                    arsenal_preset_name: arsenal_preset_name.as_deref(),
                    main_agent_cmd: &main_agent_cmd,
                    route_agent_cmd,
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
                    schedule_cron: schedule_cron.as_deref(),
                    merge_mode,
                    skill_path: skill_path.as_deref(),
                    deepsleep_cron: deepsleep_cron.as_deref(),
                },
            )
            .await?;
            if let Err(e) =
                project_setup::ensure_agents_knowledge_block(Path::new(&repo_path), &name)
            {
                tracing::warn!("setting up AGENTS.md for project {name:?} failed: {e:#}");
            }
            Response::Ok
        }
        Command::RemoveProject { .. } => {
            anyhow::bail!("project removal requires the daemon runtime")
        }
        Command::GetProjectRemoteConfig { project_id } => {
            Response::ProjectRemoteConfig(remote::get_config(pool, project_id).await?)
        }
        Command::GetProjectRemoteConfigByRepo {
            provider,
            owner,
            repo,
        } => Response::ProjectRemoteConfig(
            remote::get_config_by_repo(pool, provider, &owner, &repo).await?,
        ),
        Command::UpsertProjectRemoteConfig {
            project_id,
            provider,
            remote_url,
            owner,
            repo,
            api_base_url,
            auth_kind,
            auth_ref,
            webhook_secret_ref,
            inbound_auwsx_run_enabled,
            outbound_issue_create_enabled,
            remote_pr_merge_enabled,
            agent_comment_sync_enabled,
            subtask_comment_sync_enabled,
            finding_comment_sync_enabled,
            draft_pr_enabled,
            required_checks_policy,
            default_labels,
            default_assignees,
            pr_base_branch,
        } => {
            remote::upsert_config(
                pool,
                UpsertProjectRemoteConfig {
                    project_id,
                    provider,
                    remote_url: &remote_url,
                    owner: &owner,
                    repo: &repo,
                    api_base_url: &api_base_url,
                    auth_kind,
                    auth_ref: auth_ref.as_deref(),
                    webhook_secret_ref: webhook_secret_ref.as_deref(),
                    inbound_auwsx_run_enabled,
                    outbound_issue_create_enabled,
                    remote_pr_merge_enabled,
                    agent_comment_sync_enabled,
                    subtask_comment_sync_enabled,
                    finding_comment_sync_enabled,
                    draft_pr_enabled,
                    required_checks_policy,
                    default_labels: default_labels.as_deref(),
                    default_assignees: default_assignees.as_deref(),
                    pr_base_branch: pr_base_branch.as_deref(),
                },
                now,
            )
            .await?;
            Response::Ok
        }
        Command::DeleteProjectRemoteConfig { project_id } => {
            remote::delete_config(pool, project_id).await?;
            Response::Ok
        }
        Command::RecentRemoteSyncRuns { project_id, limit } => {
            Response::RemoteSyncRuns(remote::recent_sync_runs(pool, project_id, limit).await?)
        }
        Command::PlanIssueRemoteWorkflow { issue_id } => {
            let issue = issues::get(pool, issue_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("issue {issue_id} not found"))?;
            let config = remote::get_config(pool, issue.project_id).await?;
            let issue_link = remote::issue_link_by_issue(pool, issue_id).await?;
            let pr_link = remote::pr_link_by_issue(pool, issue_id).await?;
            let notes = crate::remote_workflow::notes_presence(pool, &issue).await?;
            Response::IssueRemoteWorkflowPlan(remote_plan::plan_issue_remote_workflow(
                RemoteWorkflowInput {
                    config: config.as_ref(),
                    issue: &issue,
                    issue_link: issue_link.as_ref(),
                    pr_link: pr_link.as_ref(),
                    notes,
                },
            ))
        }
        Command::GetIssueRemoteLinks { issue_id } => Response::IssueRemoteLinks(IssueRemoteLinks {
            issue_link: remote::issue_link_by_issue(pool, issue_id).await?,
            pr_link: remote::pr_link_by_issue(pool, issue_id).await?,
        }),
        Command::ProcessRemoteAuwsxRun {
            provider,
            delivery_id,
            event_kind,
            action,
            payload_hash,
            owner,
            repo,
            remote_issue_number,
            remote_issue_node_id,
            remote_issue_title,
            remote_issue_url,
            comment_body,
        } => {
            let outcome = remote_inbound::process_remote_auwsx_run(
                pool,
                ProcessRemoteAuwsxRunInput {
                    provider,
                    delivery_id: &delivery_id,
                    event_kind: &event_kind,
                    action: action.as_deref(),
                    payload_hash: &payload_hash,
                    owner: &owner,
                    repo: &repo,
                    remote_issue_number,
                    remote_issue_node_id: remote_issue_node_id.as_deref(),
                    remote_issue_title: &remote_issue_title,
                    remote_issue_url: &remote_issue_url,
                    comment_body: &comment_body,
                },
                now,
            )
            .await?;
            if let RemoteInboundOutcome::Accepted {
                backlog_item_id, ..
            } = &outcome
            {
                if let Some(item) = backlog::get(pool, *backlog_item_id).await? {
                    emit(
                        events,
                        Event::BacklogChanged {
                            item_id: *backlog_item_id,
                            project_id: item.project_id,
                            approval: item.approval.as_str().to_string(),
                        },
                    );
                }
            }
            Response::RemoteInboundOutcome(outcome)
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
            let created = routing::route_approved_project(pool, project_id, now)
                .await?
                .into_iter()
                .map(|outcome| outcome.issue_id())
                .collect();
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
            project_id: _,
            title: _,
            description: _,
        } => {
            anyhow::bail!("issues are agent-derived; add backlog and let routing create issues")
        }
        Command::SetIssueStatus {
            issue_id,
            status,
            force,
        } => {
            let before_issue = issues::get(pool, issue_id).await?;
            let before = before_issue
                .as_ref()
                .map(|issue| issue.status.as_str().to_string());
            let result = if before_issue
                .as_ref()
                .is_some_and(|issue| issue.status == status)
            {
                Ok(())
            } else if force {
                issues::force_status(pool, issue_id, status, now).await
            } else {
                issues::transition(pool, issue_id, status, now).await
            };
            let after = issues::get(pool, issue_id)
                .await?
                .map(|issue| issue.status.as_str().to_string());
            let log_path = agent_runs::latest_log_path_by_issue(pool, issue_id).await?;
            match &result {
                Ok(()) => {
                    append_issue_system_event(
                        log_path.as_deref(),
                        serde_json::json!({
                            "kind": "status",
                            "issue_id": issue_id,
                            "from": before,
                            "to": status.as_str(),
                            "after": after,
                            "force": force,
                            "result": "ok",
                        }),
                    );
                }
                Err(e) => {
                    append_issue_system_event(
                        log_path.as_deref(),
                        serde_json::json!({
                            "kind": "status",
                            "issue_id": issue_id,
                            "from": before,
                            "to": status.as_str(),
                            "after": after,
                            "force": force,
                            "result": "error",
                            "error": e.to_string(),
                        }),
                    );
                }
            }
            result?;
            emit(events, Event::IssueStatus { issue_id, status });
            Response::Ok
        }
        Command::ApplyIssueMerge { issue_id } => {
            apply_issue_merge(pool, events, issue_id, now).await?;
            Response::Ok
        }
        Command::RunSchedulerOnce { .. }
        | Command::ExecuteProject { .. }
        | Command::DiagnoseProject { .. }
        | Command::ReconcileProject { .. }
        | Command::ApplyReconcile { .. }
        | Command::ExecuteIssue { .. }
        | Command::RunIssueNow { .. }
        | Command::RetryIssue { .. }
        | Command::ApproveIssueMerge { .. }
        | Command::ApproveProjectMerge { .. }
        | Command::RunBacklogNow { .. }
        | Command::RemoveIssue { .. }
        | Command::AbandonIssue { .. }
        | Command::CleanupIssueWorktree { .. }
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
            output_route,
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
                    output_route,
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
            output_route,
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
                    output_route,
                    prompt: &prompt,
                    cron: &cron,
                    writable_paths: writable_paths.as_deref(),
                    enabled,
                },
            )
            .await?;
            Response::Ok
        }
        Command::RemoveRoutine { routine_id } => {
            routines::remove(pool, routine_id).await?;
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

async fn apply_issue_merge(
    pool: &sqlx::SqlitePool,
    events: &broadcast::Sender<Event>,
    issue_id: i64,
    now: i64,
) -> Result<()> {
    let issue = issues::get(pool, issue_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("issue {issue_id} not found"))?;
    if issue.status != IssueStatus::Merging {
        anyhow::bail!(
            "issue {issue_id} must be MERGING before apply-merge; current status is {}",
            issue.status.as_str()
        );
    }
    let project = projects::get(pool, issue.project_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("project {} not found", issue.project_id))?;
    if project.merge_mode != MergeMode::Local {
        anyhow::bail!(
            "issue {issue_id} belongs to project {} with non-local merge mode {}",
            project.id,
            project.merge_mode.as_str()
        );
    }
    let branch = issue
        .branch
        .clone()
        .ok_or_else(|| anyhow::anyhow!("issue {issue_id} has no branch"))?;
    let repo_path = PathBuf::from(project.repo_path);
    let log_path = agent_runs::latest_log_path_by_issue(pool, issue_id).await?;
    append_issue_system_event(
        log_path.as_deref(),
        serde_json::json!({
            "kind": "merge",
            "issue_id": issue_id,
            "branch": branch,
            "stage": "start",
            "repo_path": repo_path.display().to_string(),
        }),
    );

    let merge_issue_id = issue_id;
    let merge_branch = branch.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        local_merge::merge_issue_branch(&repo_path, merge_issue_id, &merge_branch)
    })
    .await??;

    match outcome {
        LocalMergeOutcome::Merged(result) => {
            issues::transition(pool, issue_id, IssueStatus::Done, now).await?;
            append_issue_system_event(
                log_path.as_deref(),
                serde_json::json!({
                    "kind": "merge",
                    "issue_id": issue_id,
                    "branch": result.branch,
                    "stage": "done",
                    "dirty_snapshot": result.dirty_snapshot,
                    "merge_commit": result.merge_commit,
                }),
            );
            emit(
                events,
                Event::IssueStatus {
                    issue_id,
                    status: IssueStatus::Done,
                },
            );
        }
        LocalMergeOutcome::Blocked(blocked) => {
            issues::transition(pool, issue_id, IssueStatus::ConflictBlocked, now).await?;
            append_issue_system_event(
                log_path.as_deref(),
                serde_json::json!({
                    "kind": "merge",
                    "issue_id": issue_id,
                    "branch": branch,
                    "stage": "blocked",
                    "blocked_stage": format!("{:?}", blocked.stage),
                    "dirty_snapshot": blocked.dirty_snapshot,
                    "message": blocked.message,
                }),
            );
            emit(
                events,
                Event::IssueStatus {
                    issue_id,
                    status: IssueStatus::ConflictBlocked,
                },
            );
        }
    }
    Ok(())
}

fn append_issue_system_event(log_path: Option<&str>, event: serde_json::Value) {
    let Some(path) = log_path else {
        return;
    };
    if let Err(e) = artifacts::append_system_event(Path::new(path), event) {
        tracing::warn!("writing issue system log {path} failed: {e:#}");
    }
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
            | Command::ExecuteProject { .. }
            | Command::ExecuteIssue { .. }
            | Command::RunIssueNow { .. }
            | Command::RetryIssue { .. }
            | Command::ApproveIssueMerge { .. }
            | Command::ApproveProjectMerge { .. }
            | Command::DiagnoseProject { .. }
            | Command::ReconcileProject { .. }
            | Command::ApplyReconcile { .. }
            | Command::RunBacklogNow { .. }
            | Command::RunRoutineNow { .. }
            | Command::RemoveIssue { .. }
            | Command::AbandonIssue { .. }
            | Command::CleanupIssueWorktree { .. }
            | Command::RemoveProject { .. }
            | Command::AskProject { .. }) => {
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
        Command::ExecuteProject { project_id } => {
            scheduler.execute_project(project_id, now).await?.into()
        }
        Command::ExecuteIssue { issue_id } => scheduler.execute_issue(issue_id, now).await?.into(),
        Command::RunIssueNow { issue_id } => {
            scheduler.run_issue_now(issue_id).await?;
            Response::RanIssue { issue_id }
        }
        Command::RetryIssue { issue_id } => {
            scheduler.retry_failed_issue(issue_id, now).await?;
            Response::RanIssue { issue_id }
        }
        Command::ApproveIssueMerge { issue_id } => {
            let issue_ids = scheduler.approve_issue_merge(issue_id, now).await?;
            Response::ApprovedMerge { issue_ids }
        }
        Command::ApproveProjectMerge { project_id } => {
            let issue_ids = scheduler.approve_project_merge(project_id, now).await?;
            Response::ApprovedMerge { issue_ids }
        }
        Command::DiagnoseProject { project_id } => {
            Response::ReconcileReport(scheduler.diagnose_project(project_id, true).await?)
        }
        Command::ReconcileProject {
            project_id,
            dry_run: true,
        } => Response::ReconcileReport(scheduler.diagnose_project(project_id, true).await?),
        Command::ReconcileProject {
            project_id,
            dry_run: false,
        } => Response::ReconcileReport(scheduler.reconcile_project(project_id, now).await?),
        Command::ApplyReconcile { main_job_id } => {
            Response::ReconcileReport(scheduler.apply_reconcile_job(main_job_id, now).await?)
        }
        Command::RunBacklogNow { item_id } => {
            let issue_id = scheduler.run_backlog_now(item_id, now).await?;
            Response::RanIssue { issue_id }
        }
        Command::RemoveIssue { issue_id } => {
            scheduler.remove_issue(issue_id, now).await?;
            Response::Ok
        }
        Command::AbandonIssue { issue_id } => {
            scheduler.abandon_issue(issue_id, now).await?;
            Response::Ok
        }
        Command::CleanupIssueWorktree { issue_id } => {
            scheduler.cleanup_issue_worktree_by_id(issue_id).await?;
            Response::Ok
        }
        Command::RemoveProject {
            project_id,
            shallow,
        } => {
            scheduler.remove_project(project_id, shallow).await?;
            Response::Ok
        }
        Command::AskProject {
            project_id,
            mode,
            question,
        } => {
            let answer_id = scheduler.ask_project(project_id, mode, question).await?;
            Response::Id(answer_id)
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
    let resp: Response = serde_json::from_str(&line).with_context(|| {
        "decoding daemon response; daemon protocol may be older than this client, restart the auwsx daemon"
    })?;
    Ok(with_protocol_hint(resp))
}

fn with_protocol_hint(resp: Response) -> Response {
    let Response::Err { message } = resp else {
        return resp;
    };
    if message.contains("unknown variant") {
        Response::Err {
            message: format!(
                "{message}; daemon protocol is older than this client, restart the auwsx daemon"
            ),
        }
    } else {
        Response::Err { message }
    }
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

#[cfg(test)]
mod tests {
    use super::{with_protocol_hint, Response};

    #[test]
    fn given_unknown_variant_error_when_protocol_hint_added_then_mentions_restart() {
        let resp = with_protocol_hint(Response::Err {
            message: "bad command: unknown variant `execute_issue`".to_string(),
        });

        match resp {
            Response::Err { message } => {
                assert!(message.contains("restart the auwsx daemon"));
            }
            other => panic!("expected err, got {other:?}"),
        }
    }

    #[test]
    fn given_other_error_when_protocol_hint_added_then_message_is_unchanged() {
        let resp = with_protocol_hint(Response::Err {
            message: "issue 1 is already running".to_string(),
        });

        match resp {
            Response::Err { message } => assert_eq!(message, "issue 1 is already running"),
            other => panic!("expected err, got {other:?}"),
        }
    }
}

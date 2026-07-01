//! Top-level TUI state + the async event loop.
//!
//! The TUI is a pure IPC client: it never opens the DB. It reads via
//! [`Command`] request/response and stays live by subscribing to the daemon's
//! [`Event`] stream — any event re-queries the affected view (the DB is the
//! source of truth; events are just "something changed" nudges).
//!
//! Loop: `select!` over { key presses (from a blocking reader thread), the IPC
//! event stream, and a slow redraw tick for time-based fields like `wait_until`
//! countdowns }. Terminal raw-mode + alternate-screen are entered in [`run`] and
//! always restored (normal exit, error, or panic).

use crate::input::{self, Action};
use crate::ui;
use anyhow::{Context, Result};
use auwsx_core::agent::codex;
use auwsx_core::backlog::{BacklogItem, Source};
use auwsx_core::db::agent_runs::AgentRun;
use auwsx_core::db::arsenal::ArsenalPreset;
use auwsx_core::db::ask_answers::{AskAnswer, AskMode};
use auwsx_core::db::findings::Finding;
use auwsx_core::db::global_settings::{GlobalSettings, PIPELINE_UX_GUIDANCE_MAX_CHARS};
use auwsx_core::db::issues::Issue;
use auwsx_core::db::memory_presets::MemoryPreset;
use auwsx_core::db::profiles::Profile;
use auwsx_core::db::projects::{CompletionPolicy, MergeMode, Project};
use auwsx_core::db::remote::{
    ProjectRemoteConfig, RemoteAuthKind, RemoteProvider, RemoteSyncRun, RequiredChecksPolicy,
};
use auwsx_core::db::scheduler_runs::{SchedulerRun, SchedulerRunSource};
use auwsx_core::db::subtasks::Subtask;
use auwsx_core::events::Event;
use auwsx_core::ipc::{self, Command, IssueRemoteLinks, Response};
use auwsx_core::main_jobs::MainJob;
use auwsx_core::reconcile::ProjectReconcileReport;
use auwsx_core::routines::{OutputRoute, Routine};
use auwsx_core::state::IssueStatus;
use auwsx_core::steering::Steering;
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The top-level views. `ORDER` is the user-facing tab cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Overview,
    Issue,
    Backlog,
    Logs,
    Config,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Left,
    ProjectKanban,
    IssueDetail,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSectionMode {
    Selected,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueDetailSection {
    Summary,
    Findings,
    WorkQueue,
    Log,
}

impl IssueDetailSection {
    pub const ALL: [Self; 4] = [Self::Summary, Self::Findings, Self::WorkQueue, Self::Log];

    pub fn index(self) -> usize {
        match self {
            Self::Summary => 0,
            Self::Findings => 1,
            Self::WorkQueue => 2,
            Self::Log => 3,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Summary => "Issue Detail",
            Self::Findings => "Findings",
            Self::WorkQueue => "Subtasks / Queue",
            Self::Log => "Log",
        }
    }

    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Log)
    }

    fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or(Self::Summary)
    }

    fn step(self, delta: isize) -> Self {
        let mut index = self.index();
        step(&mut index, delta, Self::ALL.len());
        Self::from_index(index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveScope {
    Project,
    Backlog,
    Issue,
}

impl View {
    pub const ORDER: [View; 4] = [View::Overview, View::Backlog, View::Logs, View::Ask];

    fn index(self) -> usize {
        Self::ORDER.iter().position(|v| *v == self).unwrap_or(0)
    }

    fn step(self, delta: isize) -> View {
        let n = Self::ORDER.len() as isize;
        let i = (self.index() as isize + delta).rem_euclid(n);
        Self::ORDER[i as usize]
    }
}

/// A node in the multi-project tree. Every item carries the owning
/// `project_id` so actions resolve against the right project regardless of the
/// cursor's vertical position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeItem {
    Project(i64),
    RoutinesRoot(i64),
    Routine { project_id: i64, id: i64 },
    BacklogRoot(i64),
    Backlog { project_id: i64, id: i64 },
    IssuesRoot(i64),
    Issue { project_id: i64, id: i64 },
    ArchiveRoot(i64),
    ArchivedIssue { project_id: i64, id: i64 },
}

impl TreeItem {
    /// The project this node belongs to.
    pub fn project_id(&self) -> i64 {
        match self {
            TreeItem::Project(id)
            | TreeItem::RoutinesRoot(id)
            | TreeItem::BacklogRoot(id)
            | TreeItem::IssuesRoot(id)
            | TreeItem::ArchiveRoot(id)
            | TreeItem::Routine { project_id: id, .. }
            | TreeItem::Backlog { project_id: id, .. }
            | TreeItem::Issue { project_id: id, .. }
            | TreeItem::ArchivedIssue { project_id: id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub item: TreeItem,
    pub label: String,
    pub depth: usize,
}

/// Eagerly-loaded children for one project. Keyed by project id in
/// [`App::children`]; refreshed wholesale on every resync.
#[derive(Default)]
pub struct ProjectChildren {
    pub routines: Vec<Routine>,
    pub backlog: Vec<BacklogItem>,
    /// Non-terminal issues shown in the main issue list and kanban.
    pub issues: Vec<Issue>,
    /// Terminal issues shown only through the low-frequency Archive section.
    pub archived_issues: Vec<Issue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsRow {
    RuntimeDefaults,
    ProfilesOverview,
    Profile(i64),
    ArsenalOverview,
    ArsenalPreset(usize),
    MemoryOverview,
    MemoryPreset(usize),
    PromptCatalog,
    PipelineUxStandard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityAction {
    Drill,
    Add,
    Edit,
    Ask,
    Remote,
    MoveMode,
    Approve,
    Delete,
    Execute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionHint {
    pub action: CapabilityAction,
    pub key: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextCapabilities {
    pub hints: Vec<ActionHint>,
}

impl ContextCapabilities {
    fn push(&mut self, action: CapabilityAction, key: impl Into<String>, label: impl Into<String>) {
        let key = key.into();
        let label = format_key_hint(&key, &label.into());
        self.hints.push(ActionHint { action, key, label });
    }

    pub fn has(&self, action: CapabilityAction) -> bool {
        self.hints.iter().any(|hint| hint.action == action)
    }
}

pub(crate) fn format_key_hint(key: &str, label: &str) -> String {
    let mut chars = label.chars();
    if key.chars().count() == 1
        && chars
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case(&key.chars().next().unwrap()))
    {
        format!("({key}){}", chars.as_str())
    } else if key == "a" && label == "steer" {
        // ^ `a` is the universal add key; steering is the issue-context add action.
        format!("({key}){label}")
    } else {
        format!("({key}) {label}")
    }
}

/// Everything the issue-detail pane shows, fetched together.
#[derive(Default)]
pub struct IssueDetail {
    pub issue: Option<Issue>,
    pub subtasks: Vec<Subtask>,
    pub findings: Vec<Finding>,
    pub steering: Vec<Steering>,
    pub runs: Vec<AgentRun>,
    pub remote_links: Option<IssueRemoteLinks>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormKind {
    Project,
    ProjectConfig,
    ProjectRemoteConfig(i64),
    ArsenalPreset,
    Backlog,
    BacklogEdit(i64),
    Routine,
    RoutineEdit(i64),
    Ask,
    QueueMessage,
    GlobalSettings,
}

#[derive(Debug, Clone)]
pub struct Form {
    pub kind: FormKind,
    pub title: &'static str,
    pub fields: Vec<FormField>,
    pub current: usize,
    pub cursor: usize,
    pub completion_sel: usize,
    pub mode: FormMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMode {
    Navigate,
    Edit,
}

#[derive(Debug, Clone)]
pub struct FormField {
    pub label: &'static str,
    pub display: &'static str,
    pub section: &'static str,
    pub help: &'static str,
    pub kind: FieldKind,
    pub value: String,
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Number { unit: Option<&'static str> },
    TextArea,
    Select { options: &'static [&'static str] },
    Combo { free_text: bool },
}

impl Form {
    fn project() -> Self {
        let repo = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Self {
            kind: FormKind::Project,
            title: "New project",
            fields: vec![
                field("name", "", false),
                field("repo_path", &repo, false),
                field("branch", "main", false),
                field("arsenal", "", false),
                // @tick = tick every daemon loop; blank/manual = manual-only.
                field("schedule_cron", "@tick", true),
            ],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn project_config(project: &Project) -> Self {
        let schedule_value = project.schedule_cron.clone();
        let deepsleep_value = project.deepsleep_cron.clone();
        Self {
            kind: FormKind::ProjectConfig,
            title: "Project config",
            fields: vec![
                field("name", &project.name, false),
                field("repo_path", &project.repo_path, false),
                field("branch", &project.default_branch, false),
                field(
                    "arsenal",
                    project.arsenal_preset_name.as_deref().unwrap_or(""),
                    false,
                ),
                field("completion", project.completion_policy.as_str(), false),
                field(
                    "plan_gate",
                    &project.plan_gate_timeout_min.to_string(),
                    false,
                ),
                field(
                    "merge_delay",
                    &project.completion_soft_timeout_min.to_string(),
                    false,
                ),
                field(
                    "iter_timeout",
                    &project.iteration_timeout_min.to_string(),
                    false,
                ),
                field(
                    "main_job_timeout",
                    &project.main_job_timeout_min.to_string(),
                    false,
                ),
                field(
                    "review_rounds",
                    &project.review_max_rounds.to_string(),
                    false,
                ),
                field(
                    "conflict_attempts",
                    &project.conflict_max_attempts.to_string(),
                    false,
                ),
                field("concurrency", &project.max_concurrency.to_string(), false),
                field(
                    "schedule_cron",
                    schedule_value.as_deref().unwrap_or(""),
                    true,
                ),
                field("merge_mode", project.merge_mode.as_str(), false),
                field(
                    "skill_path",
                    project.skill_path.as_deref().unwrap_or(""),
                    true,
                ),
                field(
                    "deepsleep_cron",
                    deepsleep_value.as_deref().unwrap_or(""),
                    false,
                ),
            ],
            current: 0,
            cursor: project.name.chars().count(),
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn project_remote_config(project_id: i64, config: Option<&ProjectRemoteConfig>) -> Self {
        let bool_value = |enabled: bool| if enabled { "true" } else { "false" };
        Self {
            kind: FormKind::ProjectRemoteConfig(project_id),
            title: "Remote repository",
            fields: vec![
                field(
                    "remote_provider",
                    config.map(|c| c.provider.as_str()).unwrap_or("github"),
                    false,
                ),
                field(
                    "remote_url",
                    config.map(|c| c.remote_url.as_str()).unwrap_or(""),
                    false,
                ),
                field(
                    "remote_owner",
                    config.map(|c| c.owner.as_str()).unwrap_or(""),
                    false,
                ),
                field(
                    "remote_repo",
                    config.map(|c| c.repo.as_str()).unwrap_or(""),
                    false,
                ),
                field(
                    "remote_api_base_url",
                    config
                        .map(|c| c.api_base_url.as_str())
                        .unwrap_or("https://api.github.com"),
                    false,
                ),
                field(
                    "remote_auth_kind",
                    config.map(|c| c.auth_kind.as_str()).unwrap_or("token_env"),
                    false,
                ),
                field(
                    "remote_auth_ref",
                    config.and_then(|c| c.auth_ref.as_deref()).unwrap_or(""),
                    true,
                ),
                field(
                    "remote_webhook_secret_ref",
                    config
                        .and_then(|c| c.webhook_secret_ref.as_deref())
                        .unwrap_or(""),
                    true,
                ),
                field(
                    "remote_inbound_auwsx_run",
                    bool_value(config.map(|c| c.inbound_auwsx_run_enabled).unwrap_or(false)),
                    false,
                ),
                field(
                    "remote_outbound_issue_create",
                    bool_value(
                        config
                            .map(|c| c.outbound_issue_create_enabled)
                            .unwrap_or(false),
                    ),
                    false,
                ),
                field(
                    "remote_pr_merge",
                    bool_value(config.map(|c| c.remote_pr_merge_enabled).unwrap_or(false)),
                    false,
                ),
                field(
                    "remote_agent_comments",
                    bool_value(
                        config
                            .map(|c| c.agent_comment_sync_enabled)
                            .unwrap_or(false),
                    ),
                    false,
                ),
                field(
                    "remote_subtask_comments",
                    bool_value(
                        config
                            .map(|c| c.subtask_comment_sync_enabled)
                            .unwrap_or(false),
                    ),
                    false,
                ),
                field(
                    "remote_finding_comments",
                    bool_value(
                        config
                            .map(|c| c.finding_comment_sync_enabled)
                            .unwrap_or(false),
                    ),
                    false,
                ),
                field(
                    "remote_draft_pr",
                    bool_value(config.map(|c| c.draft_pr_enabled).unwrap_or(false)),
                    false,
                ),
                field(
                    "remote_required_checks",
                    config
                        .map(|c| c.required_checks_policy.as_str())
                        .unwrap_or("observe"),
                    false,
                ),
                field(
                    "remote_default_labels",
                    config
                        .and_then(|c| c.default_labels.as_deref())
                        .unwrap_or(""),
                    true,
                ),
                field(
                    "remote_default_assignees",
                    config
                        .and_then(|c| c.default_assignees.as_deref())
                        .unwrap_or(""),
                    true,
                ),
                field(
                    "remote_pr_base_branch",
                    config
                        .and_then(|c| c.pr_base_branch.as_deref())
                        .unwrap_or(""),
                    true,
                ),
            ],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn arsenal_preset(preset: Option<&ArsenalPreset>) -> Self {
        let codex = codex::DEFAULT_CMD.to_string();
        Self {
            kind: FormKind::ArsenalPreset,
            title: "Arsenal preset",
            fields: vec![
                field("name", preset.map(|p| p.name.as_str()).unwrap_or(""), false),
                field(
                    "main_cmd",
                    preset.map(|p| p.main_agent_cmd.as_str()).unwrap_or(&codex),
                    false,
                ),
                field(
                    "route_cmd",
                    preset.map(|p| p.route_agent_cmd.as_str()).unwrap_or(&codex),
                    false,
                ),
                field(
                    "plan_cmd",
                    preset.map(|p| p.plan_agent_cmd.as_str()).unwrap_or(&codex),
                    false,
                ),
                field(
                    "work_cmd",
                    preset.map(|p| p.work_agent_cmd.as_str()).unwrap_or(&codex),
                    false,
                ),
                field(
                    "review_cmd",
                    preset
                        .and_then(|p| p.review_agent_cmd.as_deref())
                        .unwrap_or(""),
                    true,
                ),
            ],
            current: 0,
            cursor: preset.map(|p| p.name.chars().count()).unwrap_or_default(),
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn backlog() -> Self {
        Self {
            kind: FormKind::Backlog,
            title: "New backlog item",
            fields: vec![field("text", "", false)],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn backlog_edit(item: &BacklogItem) -> Self {
        Self {
            kind: FormKind::BacklogEdit(item.id),
            title: "Edit backlog item",
            fields: vec![field("text", &item.text, false)],
            current: 0,
            cursor: item.text.chars().count(),
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn routine() -> Self {
        Self {
            kind: FormKind::Routine,
            title: "New routine",
            fields: vec![
                field("name", "", false),
                field("output", "report", false),
                field("cron", "0 9 * * *", false),
                field("prompt", "", false),
                field("writable_paths", "", true),
                field("enabled", "true", false),
            ],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn routine_edit(routine: &Routine) -> Self {
        Self {
            kind: FormKind::RoutineEdit(routine.id),
            title: "Edit routine",
            fields: vec![
                field("name", &routine.name, false),
                field("output", routine.output_route.as_str(), false),
                field("cron", &routine.cron, false),
                field("prompt", &routine.prompt, false),
                field(
                    "writable_paths",
                    routine.writable_paths.as_deref().unwrap_or(""),
                    true,
                ),
                field(
                    "enabled",
                    if routine.enabled { "true" } else { "false" },
                    false,
                ),
            ],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn steering() -> Self {
        Self {
            kind: FormKind::QueueMessage,
            title: "New queue message",
            fields: vec![field("note", "", false)],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn ask() -> Self {
        Self {
            kind: FormKind::Ask,
            title: "Ask project",
            fields: vec![field("mode", "recall", false), field("question", "", false)],
            current: 1,
            cursor: 0,
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn global_settings(settings: Option<&GlobalSettings>) -> Self {
        Self {
            kind: FormKind::GlobalSettings,
            title: "Global settings",
            fields: vec![
                field(
                    "memory_preset",
                    settings
                        .map(|settings| settings.memory_preset_name.as_str())
                        .unwrap_or("portable-markdown"),
                    false,
                ),
                textarea_field(
                    "pipeline_ux_guidance",
                    "Worker guidance",
                    "Pipeline UX Standard",
                    "Persistent non-secret guidance injected into issue worker prompts.",
                    settings
                        .map(|settings| settings.pipeline_ux_guidance.as_str())
                        .unwrap_or(DEFAULT_PIPELINE_UX_GUIDANCE),
                    false,
                ),
            ],
            current: 0,
            cursor: 0,
            completion_sel: 0,
            mode: FormMode::Navigate,
        }
    }

    fn get(&self, label: &str) -> String {
        self.fields
            .iter()
            .find(|f| f.label == label)
            .map(|f| f.value.trim().to_string())
            .unwrap_or_default()
    }

    fn opt(&self, label: &str) -> Option<String> {
        let s = self.get(label);
        (!s.is_empty()).then_some(s)
    }

    fn set(&mut self, label: &str, value: &str) {
        let Some(idx) = self.fields.iter().position(|f| f.label == label) else {
            return;
        };
        if let Some(field) = self.fields.get_mut(idx) {
            field.value = value.to_string();
        }
        if idx == self.current {
            self.clamp_cursor();
        }
    }

    fn current_field_mut(&mut self) -> Option<&mut FormField> {
        self.fields.get_mut(self.current)
    }

    pub fn current_field(&self) -> Option<&FormField> {
        self.fields.get(self.current)
    }

    fn current_len(&self) -> usize {
        self.current_field()
            .map(|field| field.value.chars().count())
            .unwrap_or(0)
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.current_len());
    }

    fn move_field(&mut self, delta: isize) {
        let len = self.fields.len();
        if len == 0 {
            self.current = 0;
            self.cursor = 0;
            return;
        }
        self.current = if delta < 0 {
            self.current.saturating_sub(delta.unsigned_abs())
        } else {
            (self.current + delta as usize).min(len - 1)
        };
        self.cursor = self.current_len();
        self.completion_sel = 0;
    }

    fn set_current_value(&mut self, value: String) {
        if let Some(field) = self.current_field_mut() {
            field.value = value;
        }
        self.cursor = self.current_len();
        self.completion_sel = 0;
    }

    fn insert_char(&mut self, c: char) {
        let cursor = self.cursor;
        if let Some(field) = self.current_field_mut() {
            let byte_idx = char_to_byte_idx(&field.value, cursor);
            field.value.insert(byte_idx, c);
        }
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let cursor = self.cursor;
        if let Some(field) = self.current_field_mut() {
            let start = char_to_byte_idx(&field.value, cursor - 1);
            let end = char_to_byte_idx(&field.value, cursor);
            field.value.replace_range(start..end, "");
        }
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        let cursor = self.cursor;
        if cursor >= self.current_len() {
            return;
        }
        if let Some(field) = self.current_field_mut() {
            let start = char_to_byte_idx(&field.value, cursor);
            let end = char_to_byte_idx(&field.value, cursor + 1);
            field.value.replace_range(start..end, "");
        }
    }

    fn missing_required(&self) -> Option<&'static str> {
        self.fields
            .iter()
            .find(|f| !f.optional && f.value.trim().is_empty())
            .map(|f| f.label)
    }
}

const COMPLETION_OPTIONS: &[&str] = &["manual", "soft", "auto"];
const MERGE_MODE_OPTIONS: &[&str] = &["local", "pr"];
const REMOTE_PROVIDER_OPTIONS: &[&str] = &["github"];
const REMOTE_AUTH_KIND_OPTIONS: &[&str] = &["none", "token_env", "github_app"];
const REQUIRED_CHECKS_OPTIONS: &[&str] = &["observe", "require_green"];
const OUTPUT_ROUTE_OPTIONS: &[&str] = &["report", "backlog", "memory"];
const BOOL_OPTIONS: &[&str] = &["true", "false"];
const ASK_MODE_OPTIONS: &[&str] = &["recall", "seek"];
const DEFAULT_PIPELINE_UX_GUIDANCE: &str = "Build auwsx as an operator console. Derive visible actions from current capabilities, preserve focus/return context, use typed controls for closed domains, avoid duplicate paths, handle invalid/terminal states explicitly, and cover failure/restoration paths instead of only happy paths.";

fn char_to_byte_idx(value: &str, char_idx: usize) -> usize {
    value
        .char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len())
}

fn field(label: &'static str, value: &str, optional: bool) -> FormField {
    let (display, section, help, kind) = match label {
        "name" => ("Name", "Identity", "", FieldKind::Text),
        "repo_path" => (
            "Repository path",
            "Repository",
            "Path to the git repository.",
            FieldKind::Combo { free_text: true },
        ),
        "branch" => ("Default branch", "Repository", "", FieldKind::Text),
        "arsenal" => (
            "Arsenal preset",
            "Agents",
            "Pick an existing preset. Edit command templates in Settings > Arsenal.",
            FieldKind::Combo { free_text: false },
        ),
        "main_cmd" => ("Main command", "Agents", "", FieldKind::TextArea),
        "plan_cmd" => ("Plan command", "Agents", "", FieldKind::TextArea),
        "work_cmd" => ("Work command", "Agents", "", FieldKind::TextArea),
        "review_cmd" => (
            "Review command",
            "Agents",
            "Blank falls back to work command.",
            FieldKind::TextArea,
        ),
        "completion" => (
            "Completion policy",
            "Pipeline",
            "",
            FieldKind::Select {
                options: COMPLETION_OPTIONS,
            },
        ),
        "plan_gate" => (
            "Plan gate",
            "Pipeline",
            "Minutes before PLAN_READY auto-releases.",
            FieldKind::Number { unit: Some("min") },
        ),
        "merge_delay" => (
            "Merge delay",
            "Merge",
            "Minutes before READY_TO_MERGE auto-releases under soft policy.",
            FieldKind::Number { unit: Some("min") },
        ),
        "iter_timeout" => (
            "Worker timeout",
            "Pipeline",
            "",
            FieldKind::Number { unit: Some("min") },
        ),
        "main_job_timeout" => (
            "Routine timeout",
            "Pipeline",
            "",
            FieldKind::Number { unit: Some("min") },
        ),
        "review_rounds" => (
            "Review rounds",
            "Pipeline",
            "",
            FieldKind::Number { unit: None },
        ),
        "conflict_attempts" => (
            "Conflict attempts",
            "Merge",
            "",
            FieldKind::Number { unit: None },
        ),
        "concurrency" => (
            "Concurrency",
            "Scheduler",
            "Project-local maximum active issues.",
            FieldKind::Number { unit: None },
        ),
        "merge_mode" => (
            "Merge mode",
            "Merge",
            "",
            FieldKind::Select {
                options: MERGE_MODE_OPTIONS,
            },
        ),
        "remote_provider" => (
            "Provider",
            "Repository",
            "",
            FieldKind::Select {
                options: REMOTE_PROVIDER_OPTIONS,
            },
        ),
        "remote_url" => (
            "Remote URL",
            "Repository",
            "Git remote URL or canonical repository URL.",
            FieldKind::Text,
        ),
        "remote_owner" => ("Owner", "Repository", "", FieldKind::Text),
        "remote_repo" => ("Repository", "Repository", "", FieldKind::Text),
        "remote_api_base_url" => (
            "API base URL",
            "Repository",
            "Defaults to https://api.github.com.",
            FieldKind::Text,
        ),
        "remote_auth_kind" => (
            "Auth kind",
            "Auth",
            "",
            FieldKind::Select {
                options: REMOTE_AUTH_KIND_OPTIONS,
            },
        ),
        "remote_auth_ref" => (
            "Auth ref",
            "Auth",
            "Environment variable, app installation reference, or blank when auth kind is none.",
            FieldKind::Text,
        ),
        "remote_webhook_secret_ref" => (
            "Webhook secret ref",
            "Auth",
            "Reference only; do not paste the raw secret.",
            FieldKind::Text,
        ),
        "remote_inbound_auwsx_run" => (
            "/auwsx-run inbound",
            "Sync toggles",
            "Issue comments can create approved local backlog.",
            FieldKind::Select {
                options: BOOL_OPTIONS,
            },
        ),
        "remote_outbound_issue_create" => (
            "Create remote issues",
            "Sync toggles",
            "Promoted local issues can create matching remote issues.",
            FieldKind::Select {
                options: BOOL_OPTIONS,
            },
        ),
        "remote_pr_merge" => (
            "PR merge mode",
            "Sync toggles",
            "Ready local issues open/update remote PRs instead of local merges.",
            FieldKind::Select {
                options: BOOL_OPTIONS,
            },
        ),
        "remote_agent_comments" => (
            "Agent comments",
            "Comment sync",
            "Sync phase summaries to issue or PR marker comments.",
            FieldKind::Select {
                options: BOOL_OPTIONS,
            },
        ),
        "remote_subtask_comments" => (
            "Subtask comments",
            "Comment sync",
            "",
            FieldKind::Select {
                options: BOOL_OPTIONS,
            },
        ),
        "remote_finding_comments" => (
            "Finding comments",
            "Comment sync",
            "",
            FieldKind::Select {
                options: BOOL_OPTIONS,
            },
        ),
        "remote_draft_pr" => (
            "Draft PR",
            "Merge",
            "",
            FieldKind::Select {
                options: BOOL_OPTIONS,
            },
        ),
        "remote_required_checks" => (
            "Required checks",
            "Merge",
            "",
            FieldKind::Select {
                options: REQUIRED_CHECKS_OPTIONS,
            },
        ),
        "remote_default_labels" => ("Default labels", "Defaults", "Comma-separated.", FieldKind::Text),
        "remote_default_assignees" => (
            "Default assignees",
            "Defaults",
            "Comma-separated.",
            FieldKind::Text,
        ),
        "remote_pr_base_branch" => ("PR base branch", "Defaults", "", FieldKind::Text),
        "schedule_cron" => (
            "Scheduler cadence",
            "Schedule",
            "Use cron or shorthand: @tick, manual, 30m, 1h, 1d.",
            FieldKind::Text,
        ),
        "skill_path" => ("Skill path", "Knowledge", "", FieldKind::Text),
        "deepsleep_cron" => (
            "Deepsleep cadence",
            "Knowledge",
            "Use cron or shorthand. Blank/manual disables the project-owned memory routine.",
            FieldKind::Text,
        ),
        "memory_preset" => (
            "Memory preset",
            "Memory",
            "Pick an existing Memory preset. portable-markdown is local; auwsx-skills uses the configured skill stack.",
            FieldKind::Combo { free_text: false },
        ),
        "text" => ("Text", "Content", "", FieldKind::TextArea),
        "output" => (
            "Output route",
            "Output",
            "",
            FieldKind::Select {
                options: OUTPUT_ROUTE_OPTIONS,
            },
        ),
        "cron" => ("Cron", "Schedule", "", FieldKind::Text),
        "prompt" => ("Prompt", "Prompt", "", FieldKind::TextArea),
        "writable_paths" => (
            "Memory scope",
            "Safety",
            "Optional scope hint for memory routines; not source write permission.",
            FieldKind::TextArea,
        ),
        "enabled" => (
            "Enabled",
            "Schedule",
            "",
            FieldKind::Select {
                options: BOOL_OPTIONS,
            },
        ),
        "note" => ("Queue message", "Content", "", FieldKind::TextArea),
        "mode" => (
            "Mode",
            "Question",
            "",
            FieldKind::Select {
                options: ASK_MODE_OPTIONS,
            },
        ),
        "question" => ("Question", "Question", "", FieldKind::TextArea),
        _ => (label, "General", "", FieldKind::Text),
    };
    FormField {
        label,
        display,
        section,
        help,
        kind,
        value: value.to_string(),
        optional,
    }
}

fn textarea_field(
    label: &'static str,
    display: &'static str,
    section: &'static str,
    help: &'static str,
    value: &str,
    optional: bool,
) -> FormField {
    FormField {
        label,
        display,
        section,
        help,
        kind: FieldKind::TextArea,
        value: value.to_string(),
        optional,
    }
}

fn parse_i64(form: &Form, label: &'static str, status: &mut String) -> Option<i64> {
    match form.get(label).parse::<i64>() {
        Ok(value) => Some(value),
        Err(_) => {
            *status = format!("{label} must be an integer");
            None
        }
    }
}

fn parse_cadence(form: &Form, label: &'static str, status: &mut String) -> Option<Option<String>> {
    let raw = form.get(label);
    match auwsx_core::schedule::normalize_cadence_input(&raw) {
        Ok(value) => Some(value),
        Err(e) => {
            *status = format!("{label}: {e}");
            None
        }
    }
}

fn parse_bool(form: &Form, label: &'static str, status: &mut String) -> Option<bool> {
    match form.get(label).as_str() {
        "true" | "yes" | "1" | "on" => Some(true),
        "false" | "no" | "0" | "off" => Some(false),
        _ => {
            *status = format!("{label} must be true or false");
            None
        }
    }
}

fn project_tree_label(
    name: &str,
    schedule: &str,
    children: &ProjectChildren,
    expanded: bool,
) -> String {
    if expanded {
        format!("{name}  {schedule}")
    } else {
        format!(
            "{name}  {schedule}  R{} B{} I{}{}",
            children.routines.len(),
            children.backlog.len(),
            children.issues.len(),
            project_archive_count_label(children)
        )
    }
}

fn project_archive_count_label(children: &ProjectChildren) -> String {
    if children.archived_issues.is_empty() {
        String::new()
    } else {
        format!(" A{}", children.archived_issues.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectAgentConfig {
    arsenal_preset_name: String,
}

fn project_agent_config_from_form(
    form: &Form,
    preset: Option<&ArsenalPreset>,
    status: &mut String,
) -> Option<ProjectAgentConfig> {
    let requested = form.get("arsenal");
    let Some(preset) = preset else {
        *status = if requested.is_empty() {
            "select an Arsenal preset first".into()
        } else {
            format!("unknown Arsenal preset {requested}")
        };
        return None;
    };
    Some(ProjectAgentConfig {
        arsenal_preset_name: preset.name.clone(),
    })
}

fn add_project_command_from_form(
    form: &Form,
    preset: Option<&ArsenalPreset>,
    status: &mut String,
) -> Option<Command> {
    let schedule_cron = parse_cadence(form, "schedule_cron", status)?;
    let agent_config = project_agent_config_from_form(form, preset, status)?;
    Some(Command::AddProject {
        name: form.get("name"),
        repo_path: form.get("repo_path"),
        default_branch: form.get("branch"),
        arsenal_preset_name: Some(agent_config.arsenal_preset_name),
        main_agent_cmd: String::new(),
        route_agent_cmd: String::new(),
        plan_agent_cmd: String::new(),
        work_agent_cmd: String::new(),
        review_agent_cmd: None,
        completion_policy: None,
        plan_gate_timeout_min: None,
        completion_soft_timeout_min: None,
        schedule_cron,
    })
}

fn parse_output_route(form: &Form, status: &mut String) -> Option<OutputRoute> {
    match OutputRoute::from_str(&form.get("output")) {
        Some(value) => Some(value),
        None => {
            *status = "output must be report, backlog, or memory".into();
            None
        }
    }
}

fn daemon_tick_secs() -> i64 {
    std::env::var("AUWSX_TICK_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(10)
        .max(1)
}

fn issue_delete_hint(status: IssueStatus) -> &'static str {
    match status {
        IssueStatus::Done | IssueStatus::Abandoned => "d archive",
        IssueStatus::Failed => "d cleanup",
        _ => "d abandon",
    }
}

fn selected_issue_delete_hint_parts(hint: &str) -> (&str, &str) {
    hint.split_once(' ').unwrap_or(("d", "issue"))
}

pub struct App {
    pub socket: PathBuf,
    pub view: View,
    pub focus: Focus,

    pub projects: Vec<Project>,
    pub profiles: Vec<Profile>,
    pub arsenal_presets: Vec<ArsenalPreset>,
    pub memory_presets: Vec<MemoryPreset>,
    pub global_settings: Option<GlobalSettings>,
    /// Index of the "active" project — the one the cursor currently sits in.
    /// Kept in sync with `tree_sel` so the detail pane and per-project activity
    /// (recent runs, config) track the cursor across project boundaries.
    pub proj_sel: usize,
    /// Eagerly-loaded children for every project, keyed by project id.
    pub children: HashMap<i64, ProjectChildren>,
    /// Project ids whose children are expanded in the tree.
    pub expanded: HashSet<i64>,
    /// Project ids whose archive section is expanded. Archives are low-frequency
    /// UX, so expanding a project does not automatically expose terminal issues.
    pub archive_expanded: HashSet<i64>,
    pub issue_sel: usize,
    pub backlog_sel: usize,
    pub tree_sel: usize,
    pub kanban_lane_sel: usize,
    pub kanban_card_sel: usize,
    pub issue_section: IssueDetailSection,
    pub issue_section_mode: IssueSectionMode,
    pub issue_return_focus: Focus,
    pub issue_return_tree_sel: Option<usize>,
    pub move_mode: bool,
    pub detail: IssueDetail,
    pub recent_agent_runs: Vec<AgentRun>,
    pub recent_main_jobs: Vec<MainJob>,
    pub recent_scheduler_runs: Vec<SchedulerRun>,
    pub reconcile_reports: HashMap<i64, ProjectReconcileReport>,
    pub remote_configs: HashMap<i64, ProjectRemoteConfig>,
    pub remote_sync_runs: HashMap<i64, Vec<RemoteSyncRun>>,
    pub ask_answers: Vec<AskAnswer>,
    pub daemon_tick_secs: i64,
    /// Per-project epoch ms of the most recent AUTO scheduler tick.
    /// Used by the tree to render live countdowns without a per-project detail fetch.
    pub last_auto_tick: HashMap<i64, i64>,
    pub log_tail: String,
    pub log_tail_path: Option<String>,
    /// Issue log scroll offset measured from the newest visible line.
    pub issue_log_scroll: usize,
    pub selected_text_scroll_key: Option<String>,
    pub selected_text_scroll_offset: usize,
    /// Settings scroll offset for long config/prompt review content.
    pub config_scroll: usize,
    pub settings_sel: usize,

    /// Most-recent-last ring of formatted daemon events for the Logs view.
    pub log: VecDeque<String>,
    /// Whether the live event subscription is currently attached.
    pub connected: bool,
    /// A transient status/error message shown in the footer.
    pub status: String,
    pub status_until: Option<Instant>,
    /// Active inline data-entry form, rendered as a modal overlay.
    pub form: Option<Form>,
    /// When true, the quit-with-daemon confirm popup is open.
    pub confirm_quit: bool,
    pub pending_project_delete: Option<i64>,
    /// Git repos discovered under `$HOME` (display paths), for the New-project
    /// form's `repo_path` completion. Populated once by a background scan.
    pub scanned_repos: Vec<String>,
    /// Dirty flags for the terminal renderer. Normal events only redraw; layout
    /// and terminal-width-sensitive changes force a clear first.
    needs_redraw: bool,
    force_redraw: bool,
    last_poll_at: Instant,
}

const LOG_CAP: usize = 500;
const UI_POLL_INTERVAL: Duration = Duration::from_secs(5);

impl App {
    pub fn new(socket: PathBuf) -> Self {
        App {
            socket,
            view: View::Overview,
            focus: Focus::Left,
            projects: Vec::new(),
            profiles: Vec::new(),
            arsenal_presets: Vec::new(),
            memory_presets: Vec::new(),
            global_settings: None,
            proj_sel: 0,
            children: HashMap::new(),
            expanded: HashSet::new(),
            archive_expanded: HashSet::new(),
            issue_sel: 0,
            backlog_sel: 0,
            tree_sel: 0,
            kanban_lane_sel: 0,
            kanban_card_sel: 0,
            issue_section: IssueDetailSection::Summary,
            issue_section_mode: IssueSectionMode::Selected,
            issue_return_focus: Focus::Left,
            issue_return_tree_sel: None,
            move_mode: false,
            detail: IssueDetail::default(),
            recent_agent_runs: Vec::new(),
            recent_main_jobs: Vec::new(),
            recent_scheduler_runs: Vec::new(),
            reconcile_reports: HashMap::new(),
            remote_configs: HashMap::new(),
            remote_sync_runs: HashMap::new(),
            ask_answers: Vec::new(),
            daemon_tick_secs: daemon_tick_secs(),
            last_auto_tick: HashMap::new(),
            log_tail: String::new(),
            log_tail_path: None,
            issue_log_scroll: 0,
            selected_text_scroll_key: None,
            selected_text_scroll_offset: 0,
            config_scroll: 0,
            settings_sel: 0,
            log: VecDeque::new(),
            connected: false,
            status: String::new(),
            status_until: None,
            form: None,
            confirm_quit: false,
            pending_project_delete: None,
            scanned_repos: Vec::new(),
            needs_redraw: true,
            force_redraw: false,
            last_poll_at: Instant::now(),
        }
    }

    fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    fn request_force_redraw(&mut self) {
        self.needs_redraw = true;
        self.force_redraw = true;
    }

    fn render_revision(&self) -> String {
        format!(
            "{:?}|{:?}|{}|{}|{}|{}|{}|{}|{:?}|{:?}|{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}|{:?}",
            self.view,
            self.focus,
            self.proj_sel,
            self.tree_sel,
            self.issue_sel,
            self.backlog_sel,
            self.kanban_lane_sel,
            self.kanban_card_sel,
            self.issue_section,
            self.issue_section_mode,
            self.issue_return_focus,
            self.move_mode,
            self.issue_log_scroll,
            self.selected_text_scroll_key.as_deref().unwrap_or(""),
            self.selected_text_scroll_offset,
            self.config_scroll,
            self.settings_sel,
            self.connected,
            self.status,
            self.form,
            self.confirm_quit,
        )
    }

    /// Fuzzy-completion suggestions for the New-project `repo_path` field, based
    /// on the current field text. Empty unless a Project form is open on that
    /// field. Capped to keep the dropdown short.
    pub fn repo_suggestions(&self) -> Vec<String> {
        let Some(form) = &self.form else {
            return Vec::new();
        };
        if !matches!(form.kind, FormKind::Project | FormKind::ProjectConfig) {
            return Vec::new();
        }
        let Some(field) = form.fields.get(form.current) else {
            return Vec::new();
        };
        if field.label != "repo_path" {
            return Vec::new();
        }
        crate::repo_scan::filter_repos(&field.value, &self.scanned_repos, 8)
    }

    /// Preset-name completions for the `arsenal` field in project forms.
    /// Tab accepts the top match; project forms store the preset reference only.
    pub fn arsenal_suggestions(&self) -> Vec<String> {
        let Some(form) = &self.form else {
            return Vec::new();
        };
        if !matches!(form.kind, FormKind::Project | FormKind::ProjectConfig) {
            return Vec::new();
        }
        let Some(field) = form.fields.get(form.current) else {
            return Vec::new();
        };
        if field.label != "arsenal" {
            return Vec::new();
        }
        let query = field.value.trim().to_lowercase();
        self.arsenal_presets
            .iter()
            .filter(|p| query.is_empty() || p.name.to_lowercase().contains(&query))
            .take(8)
            .map(|p| p.name.clone())
            .collect()
    }

    pub fn memory_preset_suggestions(&self) -> Vec<String> {
        let Some(form) = &self.form else {
            return Vec::new();
        };
        if !matches!(form.kind, FormKind::GlobalSettings) {
            return Vec::new();
        }
        let Some(field) = form.fields.get(form.current) else {
            return Vec::new();
        };
        if field.label != "memory_preset" {
            return Vec::new();
        }
        let query = field.value.trim().to_lowercase();
        self.memory_presets
            .iter()
            .filter(|p| query.is_empty() || p.name.to_lowercase().contains(&query))
            .take(8)
            .map(|p| p.name.clone())
            .collect()
    }

    pub fn active_suggestions(&self) -> Vec<String> {
        let repo = self.repo_suggestions();
        if !repo.is_empty() {
            return repo;
        }
        let arsenal = self.arsenal_suggestions();
        if !arsenal.is_empty() {
            return arsenal;
        }
        self.memory_preset_suggestions()
    }

    pub fn selected_suggestion_index(&self) -> usize {
        let count = self.active_suggestions().len();
        if count == 0 {
            return 0;
        }
        self.form
            .as_ref()
            .map(|form| form.completion_sel.min(count - 1))
            .unwrap_or(0)
    }

    fn move_completion(&mut self, delta: isize) -> bool {
        let count = self.active_suggestions().len();
        if count == 0 {
            return false;
        }
        if let Some(form) = self.form.as_mut() {
            step(&mut form.completion_sel, delta, count);
        }
        true
    }

    fn accept_completion(&mut self) -> bool {
        let suggestions = self.active_suggestions();
        if suggestions.is_empty() {
            return false;
        }
        let idx = self.selected_suggestion_index();
        let Some(value) = suggestions.get(idx).cloned() else {
            return false;
        };
        if !self.memory_preset_suggestions().is_empty() {
            if let Some(form) = self.form.as_mut() {
                form.set_current_value(value);
            }
        } else if self.repo_suggestions().is_empty() {
            if let Some(preset) = self.find_arsenal_preset(&value) {
                if let Some(form) = self.form.as_mut() {
                    Self::select_arsenal_preset(form, &preset);
                }
            }
        } else if let Some(form) = self.form.as_mut() {
            form.set_current_value(value);
        }
        true
    }

    fn cycle_current_select(&mut self, delta: isize) -> bool {
        let Some(form) = self.form.as_mut() else {
            return false;
        };
        let Some(field) = form.current_field() else {
            return false;
        };
        let options = match &field.kind {
            FieldKind::Select { options } => *options,
            _ => return false,
        };
        if options.is_empty() {
            return false;
        }
        let current = options
            .iter()
            .position(|option| *option == field.value)
            .unwrap_or(0);
        let mut idx = current;
        step(&mut idx, delta, options.len());
        form.set_current_value(options[idx].to_string());
        true
    }

    fn find_arsenal_preset(&self, name: &str) -> Option<ArsenalPreset> {
        let needle = name.trim();
        if needle.is_empty() {
            return None;
        }
        self.arsenal_presets
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(needle))
            .cloned()
    }

    fn select_arsenal_preset(form: &mut Form, preset: &ArsenalPreset) {
        form.set("arsenal", &preset.name);
    }

    fn project_matching_arsenal(&self, project: &Project) -> Option<ArsenalPreset> {
        if let Some(name) = &project.arsenal_preset_name {
            return self.find_arsenal_preset(name);
        }
        self.arsenal_presets
            .iter()
            .find(|preset| {
                project.main_agent_cmd == preset.main_agent_cmd
                    && project.route_agent_cmd == preset.route_agent_cmd
                    && project.plan_agent_cmd == preset.plan_agent_cmd
                    && project.work_agent_cmd == preset.work_agent_cmd
                    && project.review_agent_cmd == preset.review_agent_cmd
            })
            .cloned()
    }

    fn new_project_form(&self) -> Form {
        let mut form = Form::project();
        if let Some(preset) = self
            .arsenal_presets
            .iter()
            .find(|preset| preset.name == "codex")
            .or_else(|| self.arsenal_presets.first())
        {
            Self::select_arsenal_preset(&mut form, preset);
        }
        form
    }

    fn project_config_form(&self, project: &Project) -> Form {
        let mut form = Form::project_config(project);
        if project.arsenal_preset_name.is_none() {
            if let Some(preset) = self.project_matching_arsenal(project) {
                Self::select_arsenal_preset(&mut form, &preset);
            }
        }
        form
    }

    async fn open_project_remote_form(&mut self) -> Result<()> {
        let Some(project_id) = self.selected_project_id() else {
            self.status = "select a project first".into();
            return Ok(());
        };
        self.refresh_project_remote_config(project_id).await?;
        let form = Form::project_remote_config(project_id, self.remote_configs.get(&project_id));
        self.form = Some(form);
        Ok(())
    }

    /// The "active" project id — the one the cursor currently sits in.
    pub fn selected_project_id(&self) -> Option<i64> {
        self.projects.get(self.proj_sel).map(|p| p.id)
    }

    fn children_of(&self, project_id: i64) -> Option<&ProjectChildren> {
        self.children.get(&project_id)
    }

    /// Routines of the active project (empty if none / not yet loaded).
    pub fn routines(&self) -> &[Routine] {
        self.selected_project_id()
            .and_then(|id| self.children_of(id))
            .map(|c| c.routines.as_slice())
            .unwrap_or(&[])
    }

    /// Backlog of the active project.
    pub fn backlog(&self) -> &[BacklogItem] {
        self.selected_project_id()
            .and_then(|id| self.children_of(id))
            .map(|c| c.backlog.as_slice())
            .unwrap_or(&[])
    }

    /// Issues of the active project.
    pub fn issues(&self) -> &[Issue] {
        self.selected_project_id()
            .and_then(|id| self.children_of(id))
            .map(|c| c.issues.as_slice())
            .unwrap_or(&[])
    }

    /// Archived terminal issues of the active project.
    pub fn archived_issues(&self) -> &[Issue] {
        self.selected_project_id()
            .and_then(|id| self.children_of(id))
            .map(|c| c.archived_issues.as_slice())
            .unwrap_or(&[])
    }

    pub fn remote_config(&self, project_id: i64) -> Option<&ProjectRemoteConfig> {
        self.remote_configs.get(&project_id)
    }

    pub fn remote_sync_runs(&self, project_id: i64) -> &[RemoteSyncRun] {
        self.remote_sync_runs
            .get(&project_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn active_profile_name(&self) -> &str {
        let Some(project) = self.projects.get(self.proj_sel) else {
            return "default";
        };
        self.profiles
            .iter()
            .find(|profile| profile.id == project.profile_id)
            .map(|profile| profile.name.as_str())
            .unwrap_or("default")
    }

    pub fn settings_rows(&self) -> Vec<SettingsRow> {
        let mut rows = vec![
            SettingsRow::RuntimeDefaults,
            SettingsRow::ArsenalOverview,
            SettingsRow::MemoryOverview,
            SettingsRow::ProfilesOverview,
        ];
        for profile in &self.profiles {
            rows.push(SettingsRow::Profile(profile.id));
        }
        for idx in 0..self.arsenal_presets.len() {
            rows.push(SettingsRow::ArsenalPreset(idx));
        }
        for idx in 0..self.memory_presets.len() {
            rows.push(SettingsRow::MemoryPreset(idx));
        }
        rows.push(SettingsRow::PromptCatalog);
        rows.push(SettingsRow::PipelineUxStandard);
        rows
    }

    pub fn selected_settings_row(&self) -> SettingsRow {
        let rows = self.settings_rows();
        rows.get(self.settings_sel)
            .cloned()
            .unwrap_or(SettingsRow::RuntimeDefaults)
    }

    fn move_settings_row(&mut self, delta: isize) {
        let max = self.settings_rows().len().saturating_sub(1);
        self.settings_sel = self.settings_sel.saturating_add_signed(delta).min(max);
        self.config_scroll = 0;
    }

    fn jump_settings_top(&mut self) {
        self.settings_sel = 0;
        self.config_scroll = 0;
    }

    fn jump_settings_bottom(&mut self) {
        self.settings_sel = self.settings_rows().len().saturating_sub(1);
        self.config_scroll = 0;
    }

    fn edit_selected_setting(&mut self) {
        match self.selected_settings_row() {
            SettingsRow::ArsenalPreset(idx) => {
                self.form = Some(Form::arsenal_preset(self.arsenal_presets.get(idx)));
            }
            SettingsRow::MemoryPreset(_) | SettingsRow::MemoryOverview => {
                self.form = Some(Form::global_settings(self.global_settings.as_ref()));
            }
            SettingsRow::PipelineUxStandard => {
                self.form = Some(Form::global_settings(self.global_settings.as_ref()));
            }
            SettingsRow::RuntimeDefaults => {
                self.status = "select a settings section to edit".into();
            }
            SettingsRow::ArsenalOverview => {
                self.form = Some(Form::arsenal_preset(None));
            }
            SettingsRow::ProfilesOverview => {
                self.status = "profile management is project-driven for now".into();
            }
            SettingsRow::Profile(_) => {
                self.status = "profile editing is not wired yet".into();
            }
            SettingsRow::PromptCatalog => {
                self.status = "prompt catalog is review-only".into();
            }
        }
    }

    /// The currently-selected issue id (if the cursor is on one).
    pub fn selected_issue_id(&self) -> Option<i64> {
        if self.focus == Focus::IssueDetail {
            if let Some(issue) = &self.detail.issue {
                return Some(issue.id);
            }
        }
        if self.focus == Focus::ProjectKanban {
            return match self.selected_kanban_item() {
                Some(ui::vm::KanbanItem::Issue(id)) => Some(id),
                _ => None,
            };
        }
        self.selected_issue_row_id().or_else(|| {
            (self.view == View::Issue)
                .then(|| self.issues().get(self.issue_sel).map(|i| i.id))
                .flatten()
        })
    }

    fn selected_active_issue_id(&self) -> Option<i64> {
        match self.selected_tree_item() {
            Some(TreeItem::Issue { id, .. }) => self
                .selected_issue()
                .filter(|issue| !issue.status.is_terminal())
                .map(|_| id),
            Some(TreeItem::ArchivedIssue { .. }) => None,
            _ => self
                .issues()
                .get(self.issue_sel)
                .filter(|issue| !issue.status.is_terminal())
                .map(|issue| issue.id),
        }
    }

    fn selected_issue_row_id(&self) -> Option<i64> {
        match self.selected_tree_item() {
            Some(TreeItem::Issue { id, .. } | TreeItem::ArchivedIssue { id, .. }) => Some(id),
            _ => None,
        }
    }

    pub fn tree_rows(&self) -> Vec<TreeRow> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut rows = Vec::new();
        if self.projects.is_empty() {
            return rows;
        }
        for p in &self.projects {
            let expanded = self.expanded.contains(&p.id);
            let empty = ProjectChildren::default();
            let kids = self.children.get(&p.id).unwrap_or(&empty);
            let last = self.last_auto_tick.get(&p.id).copied();
            let sched = crate::ui::schedule::tree_schedule_label(
                p.schedule_cron.as_deref(),
                last,
                p.created_at,
                now_ms,
                self.daemon_tick_secs,
            );
            rows.push(TreeRow {
                item: TreeItem::Project(p.id),
                label: project_tree_label(&p.name, &sched, kids, expanded),
                depth: 0,
            });
            if !expanded {
                continue;
            }

            rows.push(TreeRow {
                item: TreeItem::RoutinesRoot(p.id),
                label: format!("Routines  {}", kids.routines.len()),
                depth: 1,
            });
            for r in &kids.routines {
                rows.push(TreeRow {
                    item: TreeItem::Routine {
                        project_id: p.id,
                        id: r.id,
                    },
                    label: format!(
                        "{:<3} #{:<3} {}",
                        if r.enabled { "on" } else { "off" },
                        r.id,
                        r.name
                    ),
                    depth: 2,
                });
            }

            rows.push(TreeRow {
                item: TreeItem::BacklogRoot(p.id),
                label: format!("Backlog   {}", kids.backlog.len()),
                depth: 1,
            });
            for b in &kids.backlog {
                rows.push(TreeRow {
                    item: TreeItem::Backlog {
                        project_id: p.id,
                        id: b.id,
                    },
                    label: format!(
                        "{:<9} #{:<3} {}",
                        b.approval.as_str(),
                        b.id,
                        b.text.lines().next().unwrap_or("")
                    ),
                    depth: 2,
                });
            }

            rows.push(TreeRow {
                item: TreeItem::IssuesRoot(p.id),
                label: format!("Issues    {}", kids.issues.len()),
                depth: 1,
            });
            for i in &kids.issues {
                rows.push(TreeRow {
                    item: TreeItem::Issue {
                        project_id: p.id,
                        id: i.id,
                    },
                    label: crate::ui::vm::issue_tree_label_with_runs(i, &self.recent_agent_runs),
                    depth: 2,
                });
            }

            rows.push(TreeRow {
                item: TreeItem::ArchiveRoot(p.id),
                label: format!("Archive   {}", kids.archived_issues.len()),
                depth: 1,
            });
            if self.archive_expanded.contains(&p.id) {
                for i in &kids.archived_issues {
                    rows.push(TreeRow {
                        item: TreeItem::ArchivedIssue {
                            project_id: p.id,
                            id: i.id,
                        },
                        label: crate::ui::vm::issue_tree_label_with_runs(
                            i,
                            &self.recent_agent_runs,
                        ),
                        depth: 2,
                    });
                }
            }
        }
        rows
    }

    pub fn selected_tree_item(&self) -> Option<TreeItem> {
        self.tree_rows().get(self.tree_sel).map(|r| r.item.clone())
    }

    fn selected_context_item(&self) -> Option<TreeItem> {
        if self.focus == Focus::ProjectKanban {
            let project_id = self.selected_project_id()?;
            return match self.selected_kanban_item()? {
                ui::vm::KanbanItem::Backlog(id) => Some(TreeItem::Backlog { project_id, id }),
                ui::vm::KanbanItem::Issue(id) => Some(TreeItem::Issue { project_id, id }),
            };
        }
        self.selected_tree_item()
    }

    pub fn clear_expired_status(&mut self) -> bool {
        if self
            .status_until
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.status.clear();
            self.status_until = None;
            true
        } else {
            false
        }
    }

    pub fn selected_routine(&self) -> Option<&Routine> {
        match self.selected_tree_item()? {
            TreeItem::Routine { project_id, id } => self
                .children_of(project_id)?
                .routines
                .iter()
                .find(|r| r.id == id),
            _ => None,
        }
    }

    pub fn selected_backlog(&self) -> Option<&BacklogItem> {
        if self.focus == Focus::ProjectKanban {
            return match self.selected_kanban_item()? {
                ui::vm::KanbanItem::Backlog(id) => self.backlog().iter().find(|b| b.id == id),
                ui::vm::KanbanItem::Issue(_) => None,
            };
        }
        match self.selected_tree_item()? {
            TreeItem::Backlog { project_id, id } => self
                .children_of(project_id)?
                .backlog
                .iter()
                .find(|b| b.id == id),
            _ => None,
        }
    }

    pub fn selected_issue(&self) -> Option<&Issue> {
        if self.focus == Focus::IssueDetail {
            if let Some(issue) = &self.detail.issue {
                return Some(issue);
            }
        }
        if self.focus == Focus::ProjectKanban {
            return match self.selected_kanban_item()? {
                ui::vm::KanbanItem::Issue(id) => self.issues().iter().find(|i| i.id == id),
                ui::vm::KanbanItem::Backlog(_) => None,
            };
        }
        match self.selected_tree_item()? {
            TreeItem::Issue { project_id, id } => self
                .children_of(project_id)?
                .issues
                .iter()
                .find(|i| i.id == id),
            TreeItem::ArchivedIssue { project_id, id } => self
                .children_of(project_id)?
                .archived_issues
                .iter()
                .find(|i| i.id == id),
            _ if self.view == View::Issue => self.issues().get(self.issue_sel),
            _ => None,
        }
    }

    fn issue_by_id(&self, issue_id: i64) -> Option<&Issue> {
        self.children.values().find_map(|children| {
            children
                .issues
                .iter()
                .chain(children.archived_issues.iter())
                .find(|issue| issue.id == issue_id)
        })
    }

    pub fn selected_issue_delete_hint(&self) -> &'static str {
        self.selected_issue()
            .map(|issue| issue_delete_hint(issue.status))
            .unwrap_or("d issue")
    }

    pub fn selected_issue_accepts_queue_message(&self) -> bool {
        self.selected_issue()
            .is_some_and(|issue| issue.status.accepts_queue_message())
    }

    pub fn selected_issue_can_execute(&self) -> bool {
        self.selected_issue().is_some_and(|issue| {
            issue.status.is_actionable()
                || matches!(
                    issue.status,
                    IssueStatus::PlanReady
                        | IssueStatus::ReadyToMerge
                        | IssueStatus::ConflictBlocked
                        | IssueStatus::Failed
                )
        })
    }

    pub fn capabilities(&self) -> ContextCapabilities {
        let mut caps = ContextCapabilities::default();
        if self.form.is_some() || self.confirm_quit {
            return caps;
        }
        if self.view == View::Config {
            match self.selected_settings_row() {
                SettingsRow::ArsenalOverview => {
                    caps.push(CapabilityAction::Drill, "Enter", "new preset");
                    caps.push(CapabilityAction::Add, "a", "add preset");
                }
                SettingsRow::ArsenalPreset(_) => {
                    caps.push(CapabilityAction::Drill, "Enter", "edit");
                    caps.push(CapabilityAction::Edit, "e", "edit");
                }
                SettingsRow::MemoryOverview | SettingsRow::MemoryPreset(_) => {
                    caps.push(CapabilityAction::Drill, "Enter", "select");
                    caps.push(CapabilityAction::Edit, "e", "select");
                }
                SettingsRow::PipelineUxStandard => {
                    caps.push(CapabilityAction::Drill, "Enter", "edit");
                    caps.push(CapabilityAction::Edit, "e", "edit");
                }
                _ => {}
            }
            return caps;
        }
        if self.view == View::Issue {
            if self.selected_project_id().is_some() {
                caps.push(CapabilityAction::Ask, "?", "ask");
            }
            if self.selected_issue_accepts_queue_message() {
                caps.push(CapabilityAction::Add, "a", "steer");
            }
            return caps;
        }
        if self.move_mode {
            return caps;
        }
        if self.focus == Focus::IssueDetail {
            if self.selected_issue_accepts_queue_message() {
                caps.push(CapabilityAction::Add, "a", "steer");
            }
            if self.selected_issue_can_execute() {
                caps.push(CapabilityAction::Execute, "E", "execute");
            }
            if self.selected_issue().is_some() {
                let (key, label) =
                    selected_issue_delete_hint_parts(self.selected_issue_delete_hint());
                caps.push(CapabilityAction::Delete, key, label);
            }
            return caps;
        }
        if self.focus == Focus::ProjectKanban {
            if self.selected_backlog().is_some() {
                caps.push(CapabilityAction::Drill, "Enter", "select");
                caps.push(CapabilityAction::Add, "a", "add backlog");
                caps.push(CapabilityAction::Execute, "E", "execute");
                caps.push(CapabilityAction::Delete, "d", "dismiss");
            } else if self.selected_issue().is_some() {
                caps.push(CapabilityAction::Drill, "Enter", "detail");
                if self.selected_issue_accepts_queue_message() {
                    caps.push(CapabilityAction::Add, "a", "steer");
                }
                if self.selected_issue_can_execute() {
                    caps.push(CapabilityAction::Execute, "E", "execute");
                }
                let (key, label) =
                    selected_issue_delete_hint_parts(self.selected_issue_delete_hint());
                caps.push(CapabilityAction::Delete, key, label);
            }
            return caps;
        }
        if self.selected_project_id().is_some() {
            caps.push(CapabilityAction::Ask, "?", "ask");
        }
        match self.selected_tree_item() {
            Some(TreeItem::Project(_)) => {
                caps.push(CapabilityAction::Drill, "Enter", "kanban");
                caps.push(CapabilityAction::Add, "a", "add project");
                caps.push(CapabilityAction::Edit, "e", "edit");
                caps.push(CapabilityAction::Execute, "E", "execute");
                caps.push(CapabilityAction::Remote, "R", "remote");
                caps.push(CapabilityAction::MoveMode, "m", "move");
                caps.push(CapabilityAction::Delete, "d", "unregister");
            }
            Some(TreeItem::RoutinesRoot(_)) => {
                caps.push(CapabilityAction::Drill, "Enter", "fold");
                caps.push(CapabilityAction::Add, "a", "add routine");
            }
            Some(TreeItem::Routine { .. }) => {
                caps.push(CapabilityAction::Approve, "A", "toggle");
                caps.push(CapabilityAction::Edit, "e", "edit");
                caps.push(CapabilityAction::Delete, "d", "delete");
                caps.push(CapabilityAction::Execute, "E", "execute");
            }
            Some(TreeItem::BacklogRoot(_)) => {
                caps.push(CapabilityAction::Drill, "Enter", "fold");
                caps.push(CapabilityAction::Add, "a", "add backlog");
            }
            Some(TreeItem::Backlog { .. }) => {
                caps.push(CapabilityAction::Add, "a", "add backlog");
                caps.push(CapabilityAction::MoveMode, "m", "move");
                caps.push(CapabilityAction::Approve, "A", "approve");
                if self
                    .selected_backlog()
                    .is_some_and(|item| item.consumed_issue_id.is_none())
                {
                    caps.push(CapabilityAction::Edit, "e", "edit");
                    caps.push(CapabilityAction::Execute, "E", "execute");
                }
                caps.push(CapabilityAction::Delete, "d", "dismiss");
            }
            Some(TreeItem::IssuesRoot(_)) => {
                caps.push(CapabilityAction::Drill, "Enter", "fold");
            }
            Some(TreeItem::ArchiveRoot(project_id)) => {
                let label = if self.archive_expanded.contains(&project_id) {
                    "close archive"
                } else {
                    "open archive"
                };
                caps.push(CapabilityAction::Drill, "Enter", label);
            }
            Some(TreeItem::Issue { .. }) => {
                caps.push(CapabilityAction::Drill, "Enter", "detail");
                caps.push(CapabilityAction::MoveMode, "m", "move");
                if self.selected_issue_accepts_queue_message() {
                    caps.push(CapabilityAction::Add, "a", "steer");
                }
                if self.selected_issue_can_execute() {
                    caps.push(CapabilityAction::Execute, "E", "execute");
                }
                let (key, label) =
                    selected_issue_delete_hint_parts(self.selected_issue_delete_hint());
                caps.push(CapabilityAction::Delete, key, label);
            }
            Some(TreeItem::ArchivedIssue { .. }) => {
                caps.push(CapabilityAction::Drill, "Enter", "detail");
                let (key, label) =
                    selected_issue_delete_hint_parts(self.selected_issue_delete_hint());
                caps.push(CapabilityAction::Delete, key, label);
            }
            None => {
                caps.push(CapabilityAction::Add, "a", "add project");
            }
        }
        caps
    }

    pub fn selected_kanban_item(&self) -> Option<ui::vm::KanbanItem> {
        self.kanban_items_for_lane(self.kanban_lane_sel)
            .get(self.kanban_card_sel)
            .copied()
    }

    pub fn selected_kanban_item_preview(&self) -> Option<ui::vm::KanbanPreview<'_>> {
        match self.selected_kanban_item()? {
            ui::vm::KanbanItem::Backlog(id) => self
                .backlog()
                .iter()
                .find(|item| item.id == id)
                .map(ui::vm::KanbanPreview::Backlog),
            ui::vm::KanbanItem::Issue(id) => self
                .issues()
                .iter()
                .find(|issue| issue.id == id)
                .map(ui::vm::KanbanPreview::Issue),
        }
    }

    pub fn is_kanban_item_selected(&self, item: ui::vm::KanbanItem) -> bool {
        self.focus == Focus::ProjectKanban && self.selected_kanban_item() == Some(item)
    }

    pub(crate) fn kanban_cards(&self) -> Vec<ui::vm::KanbanCard> {
        ui::vm::kanban_cards_with_runs(self.backlog(), self.issues(), &self.recent_agent_runs)
    }

    fn kanban_items_for_lane(&self, lane_idx: usize) -> Vec<ui::vm::KanbanItem> {
        let lane = ui::vm::KanbanLane::ALL
            .get(lane_idx)
            .copied()
            .unwrap_or(ui::vm::KanbanLane::Plan);
        self.kanban_cards()
            .into_iter()
            .filter(|card| card.belongs_to(lane))
            .map(|card| card.item())
            .collect()
    }

    fn clamp_kanban(&mut self) {
        let lane_count = ui::vm::KanbanLane::ALL.len();
        if self.kanban_lane_sel >= lane_count {
            self.kanban_lane_sel = lane_count.saturating_sub(1);
        }
        let count = self.kanban_items_for_lane(self.kanban_lane_sel).len();
        if count == 0 {
            self.kanban_card_sel = 0;
        } else if self.kanban_card_sel >= count {
            self.kanban_card_sel = count - 1;
        }
    }

    fn select_issue_in_tree(&mut self, issue_id: i64) -> bool {
        if let Some(project_id) = self.children.iter().find_map(|(project_id, kids)| {
            kids.issues
                .iter()
                .chain(kids.archived_issues.iter())
                .any(|issue| issue.id == issue_id)
                .then_some(*project_id)
        }) {
            return self.preserve_tree_issue_selection(project_id, issue_id);
        }

        let rows = self.tree_rows();
        let Some((idx, item)) = rows
            .iter()
            .enumerate()
            .find_map(|(idx, row)| match row.item {
                TreeItem::Issue { id, .. } | TreeItem::ArchivedIssue { id, .. }
                    if id == issue_id =>
                {
                    Some((idx, row.item.clone()))
                }
                _ => None,
            })
        else {
            return false;
        };

        self.tree_sel = idx;
        if let TreeItem::Issue { project_id, id } = item {
            if let Some(idx) = self
                .children_of(project_id)
                .and_then(|kids| kids.issues.iter().position(|issue| issue.id == id))
            {
                self.issue_sel = idx;
            }
        }
        self.sync_active_project();
        true
    }

    // --- IPC helpers --------------------------------------------------------

    async fn req(&self, cmd: Command) -> Result<Response> {
        ipc::request(&self.socket, &cmd).await
    }

    /// Issue a command whose only meaningful reply is success/failure; surface
    /// any `Response::Err` in the footer instead of erroring the whole loop.
    async fn req_ok(&mut self, cmd: Command, label: &str) {
        match self.req(cmd).await {
            Ok(Response::Err { message }) => self.status = format!("{label} failed: {message}"),
            Ok(_) => self.status = format!("{label} ok"),
            Err(e) => self.status = format!("{label} failed: {e}"),
        }
    }

    // --- data refresh -------------------------------------------------------

    /// Full resync: project list + every project's children, then re-derive the
    /// active project from the cursor and freshen the detail/activity panes.
    async fn refresh_all(&mut self) -> Result<()> {
        self.refresh_global_settings().await?;
        self.refresh_arsenal().await?;
        self.refresh_memory_presets().await?;
        self.refresh_projects().await?;
        for pid in self.project_ids() {
            self.refresh_project_children(pid).await?;
        }
        self.refresh_last_auto_ticks().await?;
        self.clamp_tree();
        self.sync_active_project();
        self.refresh_detail().await?;
        self.refresh_activity().await?;
        self.refresh_asks().await?;
        Ok(())
    }

    fn project_ids(&self) -> Vec<i64> {
        self.projects.iter().map(|p| p.id).collect()
    }

    async fn refresh_projects(&mut self) -> Result<()> {
        if let Response::Profiles(profiles) = self.req(Command::ListProfiles).await? {
            self.profiles = profiles;
        }
        if let Response::Projects(ps) = self.req(Command::ListProjects).await? {
            self.projects = ps;
            if self.proj_sel >= self.projects.len() {
                self.proj_sel = self.projects.len().saturating_sub(1);
            }
            // Drop caches for projects that no longer exist; first sight of a
            // project auto-expands it so the tree is not a wall of collapsed rows.
            let live: HashSet<i64> = self.projects.iter().map(|p| p.id).collect();
            self.children.retain(|id, _| live.contains(id));
            self.remote_configs.retain(|id, _| live.contains(id));
            self.remote_sync_runs.retain(|id, _| live.contains(id));
            self.expanded.retain(|id| live.contains(id));
            self.archive_expanded.retain(|id| live.contains(id));
            if self.expanded.is_empty() {
                self.expanded.extend(live);
            }
        }
        Ok(())
    }

    async fn refresh_arsenal(&mut self) -> Result<()> {
        if let Response::ArsenalPresets(presets) = self.req(Command::ListArsenalPresets).await? {
            self.arsenal_presets = presets;
        }
        Ok(())
    }

    async fn refresh_memory_presets(&mut self) -> Result<()> {
        if let Response::MemoryPresets(presets) = self.req(Command::ListMemoryPresets).await? {
            self.memory_presets = presets;
        }
        Ok(())
    }

    async fn refresh_global_settings(&mut self) -> Result<()> {
        if let Response::GlobalSettings(settings) = self.req(Command::GetGlobalSettings).await? {
            self.global_settings = Some(settings);
        }
        Ok(())
    }

    async fn refresh_asks(&mut self) -> Result<()> {
        let Some(project_id) = self.selected_project_id() else {
            self.ask_answers.clear();
            return Ok(());
        };
        if let Response::AskAnswers(answers) = self
            .req(Command::ListAskAnswers {
                project_id,
                limit: 20,
            })
            .await?
        {
            self.ask_answers = answers;
        }
        Ok(())
    }

    async fn refresh_last_auto_ticks(&mut self) -> Result<()> {
        self.last_auto_tick.clear();
        for pid in self.project_ids() {
            self.refresh_last_auto_tick(pid).await?;
        }
        Ok(())
    }

    async fn refresh_last_auto_tick(&mut self, project_id: i64) -> Result<()> {
        if let Response::SchedulerRuns(runs) = self
            .req(Command::RecentSchedulerRunsByProject {
                project_id,
                limit: 8,
            })
            .await?
        {
            if let Some(run) = runs.iter().find(|r| r.source == SchedulerRunSource::Auto) {
                self.last_auto_tick.insert(project_id, run.fired_at);
            } else {
                self.last_auto_tick.remove(&project_id);
            }
        }
        Ok(())
    }

    /// Load routines + backlog + issues for one project into the cache.
    async fn refresh_project_children(&mut self, project_id: i64) -> Result<()> {
        self.refresh_project_remote_config(project_id).await?;
        let mut kids = ProjectChildren::default();
        if let Response::Routines(items) = self.req(Command::ListRoutines { project_id }).await? {
            kids.routines = items;
        }
        if let Response::Backlog(items) = self
            .req(Command::ListBacklog {
                project_id,
                approval: None,
            })
            .await?
        {
            kids.backlog = items;
        }
        if let Response::Issues(mut is) = self
            .req(Command::ListIssues {
                project_id,
                status: None,
            })
            .await?
        {
            let mut active = Vec::new();
            let mut archived = Vec::new();
            for issue in is.drain(..) {
                if issue.status.is_archive_status() {
                    archived.push(issue);
                } else {
                    active.push(issue);
                }
            }
            // Status view order: surface attention first, then lifecycle order.
            crate::ui::vm::sort_issues_for_status_view(&mut active);
            archived.sort_by_key(|issue| std::cmp::Reverse(issue.updated_at));
            kids.issues = active;
            kids.archived_issues = archived;
        }
        match self.req(Command::DiagnoseProject { project_id }).await {
            Ok(Response::ReconcileReport(report)) => {
                self.reconcile_reports.insert(project_id, report);
            }
            _ => {
                self.reconcile_reports.remove(&project_id);
            }
        }
        self.children.insert(project_id, kids);
        Ok(())
    }

    async fn refresh_project_remote_config(&mut self, project_id: i64) -> Result<()> {
        match self
            .req(Command::GetProjectRemoteConfig { project_id })
            .await?
        {
            Response::ProjectRemoteConfig(Some(config)) => {
                self.remote_configs.insert(project_id, config);
            }
            Response::ProjectRemoteConfig(None) => {
                self.remote_configs.remove(&project_id);
            }
            _ => {}
        }
        match self
            .req(Command::RecentRemoteSyncRuns {
                project_id,
                limit: 8,
            })
            .await?
        {
            Response::RemoteSyncRuns(runs) => {
                self.remote_sync_runs.insert(project_id, runs);
            }
            _ => {
                self.remote_sync_runs.remove(&project_id);
            }
        }
        Ok(())
    }

    /// Refresh just the active project's children (used after local mutations).
    async fn refresh_issues(&mut self) -> Result<()> {
        let selected_issue = match self.selected_tree_item() {
            Some(
                TreeItem::Issue { project_id, id } | TreeItem::ArchivedIssue { project_id, id },
            ) => Some((project_id, id)),
            _ => None,
        };
        if let Some(pid) = self.selected_project_id() {
            self.refresh_project_children(pid).await?;
        }
        let len = self.issues().len();
        if self.issue_sel >= len {
            self.issue_sel = len.saturating_sub(1);
        }
        if let Some((project_id, issue_id)) = selected_issue {
            self.preserve_tree_issue_selection(project_id, issue_id);
        }
        Ok(())
    }

    async fn refresh_backlog(&mut self) -> Result<()> {
        if let Some(pid) = self.selected_project_id() {
            self.refresh_project_children(pid).await?;
        }
        let len = self.backlog().len();
        if self.backlog_sel >= len {
            self.backlog_sel = len.saturating_sub(1);
        }
        Ok(())
    }

    async fn refresh_routines(&mut self) -> Result<()> {
        if let Some(pid) = self.selected_project_id() {
            self.refresh_project_children(pid).await?;
        }
        Ok(())
    }

    /// Re-derive `proj_sel` from the row under the cursor so the detail pane and
    /// per-project activity follow the cursor across project boundaries.
    fn sync_active_project(&mut self) {
        if let Some(item) = self.selected_tree_item() {
            let pid = item.project_id();
            if let Some(idx) = self.projects.iter().position(|p| p.id == pid) {
                self.proj_sel = idx;
            }
        }
    }

    async fn refresh_detail(&mut self) -> Result<()> {
        let Some(iid) = self.selected_issue_id() else {
            self.detail = IssueDetail::default();
            return Ok(());
        };
        let mut d = IssueDetail::default();
        if let Response::Issue(i) = self.req(Command::GetIssue { issue_id: iid }).await? {
            d.issue = i;
        }
        if let Response::Subtasks(s) = self.req(Command::ListSubtasks { issue_id: iid }).await? {
            d.subtasks = s;
        }
        if let Response::Findings(f) = self
            .req(Command::ListFindings {
                issue_id: iid,
                open_only: false,
            })
            .await?
        {
            d.findings = f;
        }
        if let Response::Steering(st) = self
            .req(Command::ListSteering {
                issue_id: iid,
                pending_only: true,
            })
            .await?
        {
            d.steering = st;
        }
        if let Response::IssueRemoteLinks(links) = self
            .req(Command::GetIssueRemoteLinks { issue_id: iid })
            .await?
        {
            d.remote_links = Some(links);
        }
        self.detail = d;
        self.refresh_issue_runs_and_tail().await?;
        Ok(())
    }

    async fn refresh_activity(&mut self) -> Result<()> {
        let Some(pid) = self.selected_project_id() else {
            self.recent_agent_runs.clear();
            self.recent_main_jobs.clear();
            return Ok(());
        };
        if let Response::AgentRuns(runs) = self
            .req(Command::RecentAgentRunsByProject {
                project_id: pid,
                limit: 8,
            })
            .await?
        {
            self.recent_agent_runs = runs;
        }
        if let Response::MainJobs(jobs) = self
            .req(Command::RecentMainJobsByProject {
                project_id: pid,
                limit: 8,
            })
            .await?
        {
            self.recent_main_jobs = jobs;
        }
        if let Response::SchedulerRuns(runs) = self
            .req(Command::RecentSchedulerRunsByProject {
                project_id: pid,
                limit: 8,
            })
            .await?
        {
            self.recent_scheduler_runs = runs;
        }
        Ok(())
    }

    async fn refresh_issue_runs_and_tail(&mut self) -> Result<()> {
        let Some(iid) = self.selected_issue_id() else {
            self.log_tail.clear();
            self.log_tail_path = None;
            self.issue_log_scroll = 0;
            return Ok(());
        };
        if let Response::AgentRuns(runs) = self
            .req(Command::ListAgentRunsByIssue { issue_id: iid })
            .await?
        {
            self.detail.runs = runs.clone();
            if let Some(run) = runs.iter().rev().find(|r| r.log_path.is_some()) {
                match self
                    .req(Command::TailAgentRunLog {
                        agent_run_id: run.id,
                        max_bytes: 128 * 1024,
                    })
                    .await
                {
                    Ok(Response::LogTail { path, text }) => {
                        if self.log_tail_path.as_deref() != Some(path.as_str()) {
                            self.issue_log_scroll = 0;
                        }
                        if self.log_tail != text
                            || self.log_tail_path.as_deref() != Some(path.as_str())
                        {
                            self.request_force_redraw();
                        }
                        self.log_tail = text;
                        self.log_tail_path = Some(path);
                    }
                    _ => {
                        self.log_tail.clear();
                        if self.log_tail_path != run.log_path {
                            self.issue_log_scroll = 0;
                        }
                        self.log_tail_path = run.log_path.clone();
                    }
                }
            } else {
                self.log_tail.clear();
                self.log_tail_path = None;
                self.issue_log_scroll = 0;
            }
            self.clamp_issue_log_scroll();
        }
        Ok(())
    }

    fn scroll_issue_log(&mut self, delta: isize) {
        let max = self.log_tail.lines().count().saturating_sub(1);
        self.issue_log_scroll = self.issue_log_scroll.saturating_add_signed(delta).min(max);
    }

    fn clamp_issue_log_scroll(&mut self) {
        let max = self.log_tail.lines().count().saturating_sub(1);
        self.issue_log_scroll = self.issue_log_scroll.min(max);
    }

    fn jump_issue_log_top(&mut self) {
        self.issue_log_scroll = self.log_tail.lines().count().saturating_sub(1);
    }

    fn jump_issue_log_bottom(&mut self) {
        self.issue_log_scroll = 0;
    }

    fn scroll_config(&mut self, delta: isize) {
        self.config_scroll = self.config_scroll.saturating_add_signed(delta).min(10_000);
    }

    pub fn selected_text_scroll_offset(&self, key: &str, total_lines: usize) -> usize {
        if total_lines <= 1 || self.selected_text_scroll_key.as_deref() != Some(key) {
            0
        } else {
            self.selected_text_scroll_offset % total_lines
        }
    }

    fn selected_text_scroll_key(&self) -> Option<String> {
        match self.selected_context_item()? {
            TreeItem::Backlog { project_id, id } => self
                .children_of(project_id)?
                .backlog
                .iter()
                .find(|item| item.id == id)
                .filter(|item| item.text.lines().count() > 1)
                .map(|_| format!("backlog:{id}")),
            TreeItem::Issue { project_id, id } => self
                .children_of(project_id)?
                .issues
                .iter()
                .find(|issue| issue.id == id)
                .and_then(|issue| issue.description.as_ref())
                .filter(|description| description.lines().count() > 1)
                .map(|_| format!("issue:{id}:description")),
            TreeItem::ArchivedIssue { project_id, id } => self
                .children_of(project_id)?
                .archived_issues
                .iter()
                .find(|issue| issue.id == id)
                .and_then(|issue| issue.description.as_ref())
                .filter(|description| description.lines().count() > 1)
                .map(|_| format!("issue:{id}:description")),
            _ => None,
        }
    }

    fn sync_selected_text_scroll(&mut self) {
        let key = self.selected_text_scroll_key();
        if self.selected_text_scroll_key != key {
            self.selected_text_scroll_key = key;
            self.selected_text_scroll_offset = 0;
        }
    }

    fn advance_selected_text_scroll(&mut self) -> bool {
        self.sync_selected_text_scroll();
        if self.selected_text_scroll_key.is_some() {
            self.selected_text_scroll_offset = self.selected_text_scroll_offset.saturating_add(1);
            true
        } else {
            false
        }
    }

    async fn ui_tick(&mut self) -> Result<bool> {
        let mut redraw = self.clear_expired_status();
        redraw |= self.advance_selected_text_scroll();
        redraw |= self.has_visible_schedule_countdown();
        if self.last_poll_at.elapsed() < UI_POLL_INTERVAL {
            return Ok(redraw);
        }
        self.last_poll_at = Instant::now();
        self.refresh_last_auto_ticks().await?;
        match self.view {
            View::Overview => {
                if !self.move_mode {
                    if let Some(project_id) = self.selected_project_id() {
                        self.refresh_project_children(project_id).await?;
                        self.refresh_activity().await?;
                    }
                }
                if self.focus == Focus::IssueDetail || self.selected_issue().is_some() {
                    self.refresh_detail().await?;
                }
            }
            View::Issue => {
                self.refresh_detail().await?;
            }
            _ => {}
        }
        Ok(true)
    }

    fn has_visible_schedule_countdown(&self) -> bool {
        self.projects.iter().any(|project| {
            auwsx_core::schedule::cadence_label(project.schedule_cron.as_deref()) != "manual"
        })
    }

    // --- action handling ----------------------------------------------------

    /// Apply one decoded action. Returns `true` when the app should quit.
    async fn apply(&mut self, action: Action) -> Result<bool> {
        self.status.clear();
        self.status_until = None;
        if !matches!(action, Action::DeleteSelected) {
            self.pending_project_delete = None;
        }
        match action {
            Action::Quit => return Ok(true),
            Action::QuitWithDaemon => {
                self.confirm_quit = true;
            }
            Action::Down => {
                if self.view == View::Issue {
                    self.scroll_issue_log(-1);
                } else if self.view == View::Config {
                    self.move_settings_row(1);
                } else if self.move_mode {
                    self.move_selected_item(1).await?;
                } else if self.focus == Focus::ProjectKanban {
                    self.move_kanban_card(1);
                } else if self.focus == Focus::IssueDetail && self.view == View::Overview {
                    if self.issue_log_section_active() {
                        self.scroll_issue_log(-1);
                    } else {
                        self.move_issue_section(1);
                    }
                } else {
                    self.move_sel(1).await?;
                }
            }
            Action::Up => {
                if self.view == View::Issue {
                    self.scroll_issue_log(1);
                } else if self.view == View::Config {
                    self.move_settings_row(-1);
                } else if self.move_mode {
                    self.move_selected_item(-1).await?;
                } else if self.focus == Focus::ProjectKanban {
                    self.move_kanban_card(-1);
                } else if self.focus == Focus::IssueDetail && self.view == View::Overview {
                    if self.issue_log_section_active() {
                        self.scroll_issue_log(1);
                    } else {
                        self.move_issue_section(-1);
                    }
                } else {
                    self.move_sel(-1).await?;
                }
            }
            Action::Left => {
                if self.move_mode {
                    self.move_selected_item_across(-1).await?;
                } else if self.focus == Focus::ProjectKanban {
                    self.move_kanban_lane(-1);
                } else if self.focus == Focus::IssueDetail && self.view == View::Overview {
                    self.move_issue_section(-1);
                }
            }
            Action::Right => {
                if self.move_mode {
                    self.move_selected_item_across(1).await?;
                } else if self.focus == Focus::ProjectKanban {
                    self.move_kanban_lane(1);
                } else if self.focus == Focus::IssueDetail && self.view == View::Overview {
                    self.move_issue_section(1);
                }
            }
            Action::PageDown => {
                if self.view == View::Issue {
                    self.scroll_issue_log(-10);
                } else if self.view == View::Config {
                    self.scroll_config(10);
                } else if self.issue_log_section_active() {
                    self.scroll_issue_log(-10);
                }
            }
            Action::PageUp => {
                if self.view == View::Issue {
                    self.scroll_issue_log(10);
                } else if self.view == View::Config {
                    self.scroll_config(-10);
                } else if self.issue_log_section_active() {
                    self.scroll_issue_log(10);
                }
            }
            Action::Top => {
                if self.view == View::Issue {
                    self.jump_issue_log_top();
                } else if self.view == View::Config {
                    self.jump_settings_top();
                } else if self.issue_log_section_active() {
                    self.jump_issue_log_top();
                }
            }
            Action::Bottom => {
                if self.view == View::Issue {
                    self.jump_issue_log_bottom();
                } else if self.view == View::Config {
                    self.jump_settings_bottom();
                } else if self.issue_log_section_active() {
                    self.jump_issue_log_bottom();
                }
            }
            Action::Drill => {
                if self.view == View::Overview && self.focus == Focus::IssueDetail {
                    self.activate_issue_section();
                    return Ok(false);
                }
                if !self.capabilities().has(CapabilityAction::Drill) {
                    self.status = "nothing to open here".into();
                    return Ok(false);
                }
                if self.view == View::Config {
                    self.edit_selected_setting();
                } else {
                    self.drill().await?;
                }
            }
            Action::NextView => self.set_view(self.view.step(1)).await?,
            Action::PrevView => self.set_view(self.view.step(-1)).await?,
            Action::Back => {
                if self.view == View::Overview && self.focus == Focus::IssueDetail {
                    if self.issue_section_is_active() {
                        self.issue_section_mode = IssueSectionMode::Selected;
                        return Ok(false);
                    }
                    if self.issue_return_focus == Focus::ProjectKanban {
                        if let Some(tree_sel) = self.issue_return_tree_sel.take() {
                            self.tree_sel = tree_sel;
                            self.sync_active_project();
                        }
                    }
                    self.focus = self.issue_return_focus;
                    self.move_mode = false;
                } else if self.view == View::Overview && self.focus == Focus::ProjectKanban {
                    self.focus = Focus::Left;
                    self.move_mode = false;
                } else if matches!(self.view, View::Config | View::Issue) {
                    self.set_view(View::Overview).await?;
                }
            }
            Action::Add => self.open_context_add_form(),
            Action::Ask => {
                if !self.capabilities().has(CapabilityAction::Ask) {
                    self.status = "select a project context before asking".into();
                    return Ok(false);
                }
                if self.selected_project_id().is_some() {
                    self.form = Some(Form::ask());
                } else {
                    self.status = "select or create a project first".into();
                }
            }
            Action::EditSelected => {
                if !self.capabilities().has(CapabilityAction::Edit) {
                    self.status = "nothing editable here".into();
                    return Ok(false);
                }
                if self.view == View::Config {
                    self.edit_selected_setting();
                } else {
                    match self.selected_context_item() {
                        Some(TreeItem::Project(_)) => {
                            if let Some(project) = self.projects.get(self.proj_sel) {
                                self.form = Some(self.project_config_form(project));
                            } else {
                                self.status = "select or create a project first".into();
                            }
                        }
                        Some(TreeItem::Backlog { .. }) => {
                            if let Some(item) = self.selected_backlog() {
                                if item.consumed_issue_id.is_some() {
                                    self.status = "consumed backlog cannot be edited".into();
                                } else {
                                    self.form = Some(Form::backlog_edit(item));
                                }
                            }
                        }
                        Some(TreeItem::Routine { .. }) => {
                            if let Some(routine) = self.selected_routine() {
                                self.form = Some(Form::routine_edit(routine));
                            }
                        }
                        _ => {
                            self.status = "select project, backlog item, or routine to edit".into()
                        }
                    }
                }
            }
            Action::Settings => self.set_view(View::Config).await?,
            Action::RemoteConfig => {
                if !self.capabilities().has(CapabilityAction::Remote) {
                    self.status = "select a project row before editing remote settings".into();
                    return Ok(false);
                }
                self.open_project_remote_form().await?;
            }
            Action::MoveMode => {
                if self.move_mode {
                    self.move_mode = false;
                    self.status = "move mode off".into();
                    return Ok(false);
                }
                if !self.capabilities().has(CapabilityAction::MoveMode) {
                    self.status = "select a movable project, backlog item, or issue first".into();
                    return Ok(false);
                }
                if let Some(scope) = self.selected_move_scope() {
                    self.move_mode = true;
                    self.focus = Focus::Left;
                    self.status = match scope {
                        MoveScope::Project => {
                            "move mode: project j/k reorder, h/l move profile, m exits".into()
                        }
                        MoveScope::Backlog => {
                            "move mode: backlog j/k reorder loaded list, m exits".into()
                        }
                        MoveScope::Issue => {
                            "move mode: issue j/k reorder loaded list, m exits".into()
                        }
                    };
                } else {
                    self.status = "select a movable project, backlog item, or issue first".into();
                }
            }
            Action::PrevProject => self.select_adjacent_project(-1).await?,
            Action::NextProject => self.select_adjacent_project(1).await?,
            Action::ApproveOrToggle => {
                if !self.capabilities().has(CapabilityAction::Approve) {
                    self.status = "nothing approvable here".into();
                    return Ok(false);
                }
                match self.selected_context_item() {
                    Some(TreeItem::Backlog { id, .. }) => {
                        self.req_ok(Command::ApproveBacklog { item_id: id }, "approve")
                            .await;
                        self.refresh_backlog().await?;
                    }
                    Some(TreeItem::Routine { .. }) => {
                        self.toggle_selected_routine().await?;
                    }
                    _ => self.status = "select backlog item or routine first".into(),
                }
            }
            Action::DeleteSelected => {
                if !self.capabilities().has(CapabilityAction::Delete) {
                    self.status = "nothing removable here".into();
                    return Ok(false);
                }
                match self.selected_context_item() {
                    Some(TreeItem::Backlog { id, .. }) => {
                        self.req_ok(Command::DismissBacklog { item_id: id }, "dismiss")
                            .await;
                        self.refresh_backlog().await?;
                    }
                    Some(TreeItem::Issue { id, .. } | TreeItem::ArchivedIssue { id, .. }) => {
                        let (cmd, label) = self.issue_delete_command(id);
                        self.req_ok(cmd, label).await;
                        self.refresh_issues().await?;
                        self.refresh_backlog().await?;
                    }
                    Some(TreeItem::Project(_)) => {
                        if let Some(project) = self.projects.get(self.proj_sel).cloned() {
                            if self.pending_project_delete == Some(project.id) {
                                self.req_ok(
                                    Command::RemoveProject {
                                        project_id: project.id,
                                        shallow: true,
                                    },
                                    "unregister project",
                                )
                                .await;
                                self.pending_project_delete = None;
                                self.refresh_all().await?;
                            } else {
                                self.pending_project_delete = Some(project.id);
                                self.status = format!(
                                    "press d again to shallow unregister {} (running work/worktrees are not cleaned)",
                                    project.name
                                );
                            }
                        }
                    }
                    Some(TreeItem::Routine { id, .. }) => {
                        self.req_ok(Command::RemoveRoutine { routine_id: id }, "delete routine")
                            .await;
                        self.refresh_routines().await?;
                    }
                    _ => self.status = "select backlog, issue, routine, or project first".into(),
                }
            }
            Action::Execute => {
                if !self.capabilities().has(CapabilityAction::Execute) {
                    self.status = "nothing runnable here".into();
                    return Ok(false);
                }
                self.execute_selected().await?;
            }
        }
        Ok(false)
    }

    fn issue_delete_command(&self, issue_id: i64) -> (Command, &'static str) {
        let status = self.issue_by_id(issue_id).map(|issue| issue.status);
        match status {
            Some(IssueStatus::Done | IssueStatus::Abandoned) => {
                (Command::CleanupIssueWorktree { issue_id }, "archive issue")
            }
            Some(IssueStatus::Failed) => {
                (Command::CleanupIssueWorktree { issue_id }, "cleanup issue")
            }
            _ => (Command::AbandonIssue { issue_id }, "abandon issue"),
        }
    }

    async fn execute_selected(&mut self) -> Result<()> {
        match self.selected_context_item() {
            Some(TreeItem::Project(project_id)) => {
                self.execute_control(Command::ReconcileProject {
                    project_id,
                    dry_run: false,
                })
                .await;
            }
            Some(TreeItem::Backlog { id: item_id, .. }) => {
                self.run_now(Command::RunBacklogNow { item_id }).await;
            }
            Some(TreeItem::Issue { id: issue_id, .. }) => {
                if self
                    .selected_issue()
                    .is_some_and(|issue| issue.status == IssueStatus::Failed)
                {
                    self.execute_control(Command::RetryIssue { issue_id }).await;
                } else {
                    self.execute_control(Command::ExecuteIssue { issue_id })
                        .await;
                }
            }
            Some(TreeItem::ArchivedIssue { .. }) => {
                self.status = "archived issue is terminal; no run action available".into();
            }
            Some(TreeItem::Routine { id: routine_id, .. }) => {
                self.req_ok(Command::RunRoutineNow { routine_id }, "routine run")
                    .await;
            }
            _ => self.status = "select a project, backlog item, issue, or routine".into(),
        }
        self.refresh_all().await?;
        Ok(())
    }

    async fn execute_control(&mut self, cmd: Command) {
        match self.req(cmd).await {
            Ok(Response::Ok) => self.status = "scheduler tick ok".into(),
            Ok(Response::RanIssue { issue_id }) => {
                self.status = format!("running issue #{issue_id}");
            }
            Ok(Response::ApprovedMerge { issue_ids }) => {
                let joined = issue_ids
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.status = format!("approved merge for {joined}");
            }
            Ok(Response::ReconcileReport(report)) => {
                self.status = format!(
                    "reconcile: applied {} queued {} manual {} agentic {}",
                    report.applied_count,
                    report
                        .queued_main_job_id
                        .map(|id| format!("#{id}"))
                        .unwrap_or_else(|| "-".to_string()),
                    report.manual_count,
                    report.agentic_count
                );
            }
            Ok(Response::Err { message }) => self.status = format!("execute failed: {message}"),
            Ok(_) => self.status = "execute failed: unexpected response".into(),
            Err(e) => self.status = format!("execute failed: {e}"),
        }
    }

    async fn run_now(&mut self, cmd: Command) {
        match self.req(cmd).await {
            Ok(Response::RanIssue { issue_id }) => {
                self.status = format!("running issue #{issue_id}");
            }
            Ok(Response::Err { message }) => self.status = format!("run failed: {message}"),
            Ok(_) => self.status = "run failed: unexpected response".into(),
            Err(e) => self.status = format!("run failed: {e}"),
        }
    }

    async fn toggle_selected_routine(&mut self) -> Result<()> {
        let Some(r) = self.selected_routine() else {
            return Ok(());
        };
        self.req_ok(
            Command::ToggleRoutine {
                routine_id: r.id,
                enabled: !r.enabled,
            },
            "toggle routine",
        )
        .await;
        self.refresh_routines().await?;
        Ok(())
    }

    pub async fn handle_form_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.form.is_none() {
            return Ok(());
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.form = None;
            return Ok(());
        }

        let mode = self.form.as_ref().map(|form| form.mode);
        if matches!(mode, Some(FormMode::Navigate)) {
            return self.handle_form_navigation_key(key).await;
        }
        self.handle_form_edit_key(key).await
    }

    async fn handle_form_navigation_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.form = None;
                self.status = "cancelled".into();
                self.status_until = Some(Instant::now() + Duration::from_millis(1500));
            }
            KeyCode::Char('E') => self.submit_form().await?,
            KeyCode::Tab if self.accept_completion() => {}
            KeyCode::Down if self.move_completion(1) => {}
            KeyCode::Up if self.move_completion(-1) => {}
            KeyCode::Enter => {
                if self.current_form_field_accepts_input() {
                    if let Some(form) = self.form.as_mut() {
                        form.mode = FormMode::Edit;
                        form.clamp_cursor();
                    }
                } else {
                    self.status = "field uses h/l or prefix keys".into();
                }
            }
            KeyCode::Left if self.cycle_current_select(-1) => {}
            KeyCode::Right if self.cycle_current_select(1) => {}
            KeyCode::Char(' ') if self.cycle_current_select(1) => {}
            KeyCode::Char(c)
                if matches!(
                    self.form
                        .as_ref()
                        .and_then(Form::current_field)
                        .map(|field| &field.kind),
                    Some(FieldKind::Select { .. })
                ) =>
            {
                self.select_option_by_prefix(c);
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(form) = self.form.as_mut() {
                    form.move_field(1);
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(form) = self.form.as_mut() {
                    form.move_field(-1);
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_form_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                if let Some(form) = self.form.as_mut() {
                    form.mode = FormMode::Navigate;
                }
            }
            KeyCode::Tab if self.accept_completion() => {}
            KeyCode::Down if self.move_completion(1) => {}
            KeyCode::Up if self.move_completion(-1) => {}
            KeyCode::Left => {
                if let Some(form) = self.form.as_mut() {
                    form.cursor = form.cursor.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if let Some(form) = self.form.as_mut() {
                    form.cursor = (form.cursor + 1).min(form.current_len());
                }
            }
            KeyCode::Home => {
                if let Some(form) = self.form.as_mut() {
                    form.cursor = 0;
                }
            }
            KeyCode::End => {
                if let Some(form) = self.form.as_mut() {
                    form.cursor = form.current_len();
                }
            }
            KeyCode::Backspace => {
                if let Some(form) = self.form.as_mut() {
                    form.backspace();
                }
            }
            KeyCode::Delete => {
                if let Some(form) = self.form.as_mut() {
                    form.delete();
                }
            }
            KeyCode::Char(c) => {
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                    let editable = self
                        .form
                        .as_ref()
                        .and_then(Form::current_field)
                        .is_some_and(|field| {
                            !matches!(
                                field.kind,
                                FieldKind::Select { .. } | FieldKind::Combo { free_text: false }
                            )
                        });
                    if editable {
                        if let Some(form) = self.form.as_mut() {
                            form.insert_char(c);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn current_form_field_accepts_input(&self) -> bool {
        self.form
            .as_ref()
            .and_then(Form::current_field)
            .is_some_and(|field| {
                !matches!(
                    field.kind,
                    FieldKind::Select { .. } | FieldKind::Combo { free_text: false }
                )
            })
    }

    fn select_option_by_prefix(&mut self, c: char) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(field) = form.current_field() else {
            return;
        };
        let options = match &field.kind {
            FieldKind::Select { options } => *options,
            _ => return,
        };
        let needle = c.to_ascii_lowercase();
        let Some(option) = options
            .iter()
            .find(|option| option.starts_with(needle))
            .copied()
        else {
            return;
        };
        form.set_current_value(option.to_string());
    }

    /// Handle a key while the quit-confirm popup is open. Returns Ok(true) when
    /// the TUI should exit. `y`/Enter confirms (stop daemon, then quit);
    /// `n`/Esc cancels; Ctrl-C quits the TUI without stopping the daemon.
    pub async fn handle_confirm_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.confirm_quit = false;
            return Ok(true); // hard quit, daemon left running
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                // Best-effort graceful daemon shutdown, then quit regardless.
                let _ = self.req(Command::Shutdown).await;
                self.confirm_quit = false;
                Ok(true)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.confirm_quit = false;
                Ok(false)
            }
            _ => Ok(false), // ignore other keys; popup stays open
        }
    }

    async fn submit_form(&mut self) -> Result<()> {
        let Some(form) = self.form.clone() else {
            return Ok(());
        };
        if let Some(label) = form.missing_required() {
            self.status = format!("{label} is required");
            return Ok(());
        }

        match form.kind {
            FormKind::Project => {
                let preset = self.find_arsenal_preset(&form.get("arsenal"));
                let Some(cmd) =
                    add_project_command_from_form(&form, preset.as_ref(), &mut self.status)
                else {
                    return Ok(());
                };
                self.submit_create(cmd, "project").await?;
                self.refresh_projects().await?;
                self.proj_sel = self.projects.len().saturating_sub(1);
                if let Some(pid) = self.selected_project_id() {
                    self.expanded.insert(pid);
                    self.refresh_project_children(pid).await?;
                }
                // Drop the cursor onto the new project's header row.
                if let Some(pid) = self.selected_project_id() {
                    if let Some(idx) = self
                        .tree_rows()
                        .iter()
                        .position(|r| r.item == TreeItem::Project(pid))
                    {
                        self.tree_sel = idx;
                    }
                }
            }
            FormKind::ProjectConfig => {
                let Some(project_id) = self.selected_project_id() else {
                    self.status = "select a project first".into();
                    return Ok(());
                };
                let Some(completion_policy) = CompletionPolicy::from_str(&form.get("completion"))
                else {
                    self.status = "completion must be manual, soft, or auto".into();
                    return Ok(());
                };
                let Some(plan_gate_timeout_min) = parse_i64(&form, "plan_gate", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(completion_soft_timeout_min) =
                    parse_i64(&form, "merge_delay", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(iteration_timeout_min) =
                    parse_i64(&form, "iter_timeout", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(main_job_timeout_min) =
                    parse_i64(&form, "main_job_timeout", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(review_max_rounds) = parse_i64(&form, "review_rounds", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(conflict_max_attempts) =
                    parse_i64(&form, "conflict_attempts", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(max_concurrency) = parse_i64(&form, "concurrency", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(schedule_cron) = parse_cadence(&form, "schedule_cron", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(merge_mode) = MergeMode::from_str(&form.get("merge_mode")) else {
                    self.status = "merge_mode must be local or pr".into();
                    return Ok(());
                };
                let Some(deepsleep_cron) = parse_cadence(&form, "deepsleep_cron", &mut self.status)
                else {
                    return Ok(());
                };
                let preset = self.find_arsenal_preset(&form.get("arsenal"));
                let Some(agent_config) =
                    project_agent_config_from_form(&form, preset.as_ref(), &mut self.status)
                else {
                    return Ok(());
                };
                self.req_ok(
                    Command::UpdateProject {
                        project_id,
                        name: form.get("name"),
                        repo_path: form.get("repo_path"),
                        default_branch: form.get("branch"),
                        arsenal_preset_name: Some(agent_config.arsenal_preset_name),
                        main_agent_cmd: String::new(),
                        route_agent_cmd: String::new(),
                        plan_agent_cmd: String::new(),
                        work_agent_cmd: String::new(),
                        review_agent_cmd: None,
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
                        skill_path: form.opt("skill_path"),
                        deepsleep_cron,
                    },
                    "project update",
                )
                .await;
                self.form = None;
                self.refresh_projects().await?;
            }
            FormKind::ProjectRemoteConfig(project_id) => {
                let Some(provider) = RemoteProvider::from_str(&form.get("remote_provider")) else {
                    self.status = "provider must be github".into();
                    return Ok(());
                };
                let Some(auth_kind) = RemoteAuthKind::from_str(&form.get("remote_auth_kind"))
                else {
                    self.status = "auth kind must be none, token_env, or github_app".into();
                    return Ok(());
                };
                let Some(required_checks_policy) =
                    RequiredChecksPolicy::from_str(&form.get("remote_required_checks"))
                else {
                    self.status = "required checks must be observe or require_green".into();
                    return Ok(());
                };
                let Some(inbound_auwsx_run_enabled) =
                    parse_bool(&form, "remote_inbound_auwsx_run", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(outbound_issue_create_enabled) =
                    parse_bool(&form, "remote_outbound_issue_create", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(remote_pr_merge_enabled) =
                    parse_bool(&form, "remote_pr_merge", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(agent_comment_sync_enabled) =
                    parse_bool(&form, "remote_agent_comments", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(subtask_comment_sync_enabled) =
                    parse_bool(&form, "remote_subtask_comments", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(finding_comment_sync_enabled) =
                    parse_bool(&form, "remote_finding_comments", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(draft_pr_enabled) = parse_bool(&form, "remote_draft_pr", &mut self.status)
                else {
                    return Ok(());
                };
                if auth_kind != RemoteAuthKind::None && form.opt("remote_auth_ref").is_none() {
                    self.status = "auth ref is required unless auth kind is none".into();
                    return Ok(());
                }
                self.req_ok(
                    Command::UpsertProjectRemoteConfig {
                        project_id,
                        provider,
                        remote_url: form.get("remote_url"),
                        owner: form.get("remote_owner"),
                        repo: form.get("remote_repo"),
                        api_base_url: form.get("remote_api_base_url"),
                        auth_kind,
                        auth_ref: form.opt("remote_auth_ref"),
                        webhook_secret_ref: form.opt("remote_webhook_secret_ref"),
                        inbound_auwsx_run_enabled,
                        outbound_issue_create_enabled,
                        remote_pr_merge_enabled,
                        agent_comment_sync_enabled,
                        subtask_comment_sync_enabled,
                        finding_comment_sync_enabled,
                        draft_pr_enabled,
                        required_checks_policy,
                        default_labels: form.opt("remote_default_labels"),
                        default_assignees: form.opt("remote_default_assignees"),
                        pr_base_branch: form.opt("remote_pr_base_branch"),
                    },
                    "remote config",
                )
                .await;
                self.form = None;
                self.refresh_project_remote_config(project_id).await?;
            }
            FormKind::ArsenalPreset => {
                self.req_ok(
                    Command::UpsertArsenalPreset {
                        name: form.get("name"),
                        main_agent_cmd: form.get("main_cmd"),
                        route_agent_cmd: form.get("route_cmd"),
                        plan_agent_cmd: form.get("plan_cmd"),
                        work_agent_cmd: form.get("work_cmd"),
                        review_agent_cmd: form.opt("review_cmd"),
                    },
                    "arsenal preset",
                )
                .await;
                self.form = None;
                self.refresh_arsenal().await?;
            }
            FormKind::GlobalSettings => {
                if self
                    .memory_presets
                    .iter()
                    .all(|preset| preset.name != form.get("memory_preset"))
                {
                    self.status = format!("unknown Memory preset {}", form.get("memory_preset"));
                    return Ok(());
                }
                if form.get("pipeline_ux_guidance").chars().count() > PIPELINE_UX_GUIDANCE_MAX_CHARS
                {
                    self.status = format!(
                        "pipeline UX guidance must be at most {} characters",
                        PIPELINE_UX_GUIDANCE_MAX_CHARS
                    );
                    return Ok(());
                }
                self.req_ok(
                    Command::UpdateGlobalSettings {
                        memory_preset_name: form.get("memory_preset"),
                        pipeline_ux_guidance: form.get("pipeline_ux_guidance"),
                    },
                    "global settings",
                )
                .await;
                self.form = None;
                self.refresh_global_settings().await?;
            }
            FormKind::Backlog => {
                let Some(project_id) = self.selected_project_id() else {
                    self.status = "select a project first".into();
                    return Ok(());
                };
                self.submit_create(
                    Command::AddBacklog {
                        project_id,
                        text: form.get("text"),
                        source: Source::Human,
                    },
                    "backlog item",
                )
                .await?;
                self.refresh_backlog().await?;
            }
            FormKind::BacklogEdit(item_id) => {
                self.req_ok(
                    Command::UpdateBacklogText {
                        item_id,
                        text: form.get("text"),
                    },
                    "backlog update",
                )
                .await;
                self.form = None;
                self.refresh_backlog().await?;
            }
            FormKind::Routine => {
                let Some(project_id) = self.selected_project_id() else {
                    self.status = "select a project first".into();
                    return Ok(());
                };
                let Some(output_route) = parse_output_route(&form, &mut self.status) else {
                    return Ok(());
                };
                let Some(enabled) = parse_bool(&form, "enabled", &mut self.status) else {
                    return Ok(());
                };
                self.submit_create(
                    Command::CreateRoutine {
                        project_id,
                        name: form.get("name"),
                        output_route,
                        prompt: form.get("prompt"),
                        cron: form.get("cron"),
                        writable_paths: form.opt("writable_paths"),
                        enabled,
                    },
                    "routine",
                )
                .await?;
                self.refresh_routines().await?;
            }
            FormKind::RoutineEdit(routine_id) => {
                let Some(output_route) = parse_output_route(&form, &mut self.status) else {
                    return Ok(());
                };
                let Some(enabled) = parse_bool(&form, "enabled", &mut self.status) else {
                    return Ok(());
                };
                self.req_ok(
                    Command::UpdateRoutine {
                        routine_id,
                        name: form.get("name"),
                        output_route,
                        prompt: form.get("prompt"),
                        cron: form.get("cron"),
                        writable_paths: form.opt("writable_paths"),
                        enabled,
                    },
                    "routine update",
                )
                .await;
                self.form = None;
                self.refresh_routines().await?;
            }
            FormKind::Ask => {
                let Some(project_id) = self.selected_project_id() else {
                    self.status = "select a project first".into();
                    return Ok(());
                };
                let mode = match form.get("mode").as_str() {
                    "recall" => AskMode::Recall,
                    "seek" => AskMode::Seek,
                    _ => {
                        self.status = "mode must be recall or seek".into();
                        return Ok(());
                    }
                };
                self.submit_create(
                    Command::AskProject {
                        project_id,
                        mode,
                        question: form.get("question"),
                    },
                    "answer",
                )
                .await?;
                self.refresh_asks().await?;
            }
            FormKind::QueueMessage => {
                let Some(issue_id) = self.selected_active_issue_id() else {
                    self.status = "select an active issue first".into();
                    return Ok(());
                };
                self.submit_create(
                    Command::AddSteering {
                        issue_id,
                        source: auwsx_core::steering::SteeringSource::Human,
                        note: form.get("note"),
                    },
                    "queue message",
                )
                .await?;
                self.refresh_detail().await?;
            }
        }
        Ok(())
    }

    async fn submit_create(&mut self, cmd: Command, label: &str) -> Result<()> {
        match self.req(cmd).await {
            Ok(Response::Id(id)) => {
                self.form = None;
                self.status = format!("created {label} #{id}");
            }
            Ok(Response::Ok) => {
                self.form = None;
                self.status = format!("created {label}");
            }
            Ok(Response::Err { message }) => self.status = format!("{label} failed: {message}"),
            Ok(_) => self.status = format!("{label} failed: unexpected response"),
            Err(e) => self.status = format!("{label} failed: {e}"),
        }
        Ok(())
    }

    async fn set_view(&mut self, v: View) -> Result<()> {
        self.view = v;
        if v == View::Config {
            self.config_scroll = 0;
            self.settings_sel = self
                .settings_sel
                .min(self.settings_rows().len().saturating_sub(1));
        }
        self.focus = match v {
            View::Overview => Focus::Left,
            View::Config => Focus::Settings,
            View::Issue => Focus::IssueDetail,
            _ => Focus::Left,
        };
        // Entering a view freshens exactly what it shows.
        match v {
            View::Issue => self.refresh_detail().await?,
            View::Backlog => self.refresh_backlog().await?,
            View::Config => {
                self.refresh_arsenal().await?;
                self.refresh_memory_presets().await?;
                self.refresh_global_settings().await?;
            }
            View::Ask => self.refresh_asks().await?,
            _ => {}
        }
        Ok(())
    }

    async fn move_sel(&mut self, delta: isize) -> Result<()> {
        let len = self.tree_rows().len();
        step(&mut self.tree_sel, delta, len);
        // The detail pane and per-project activity track whichever project the
        // cursor now sits in.
        self.sync_active_project();
        self.sync_selected_text_scroll();
        self.refresh_asks().await?;
        if let Some(TreeItem::Issue { id, .. } | TreeItem::ArchivedIssue { id, .. }) =
            self.selected_tree_item()
        {
            if let Some(idx) = self.issues().iter().position(|i| i.id == id) {
                self.issue_sel = idx;
            }
            self.refresh_detail().await?;
        } else {
            self.refresh_activity().await?;
        }
        Ok(())
    }

    async fn select_adjacent_project(&mut self, delta: isize) -> Result<()> {
        if self.projects.is_empty() {
            self.status = "no projects".into();
            return Ok(());
        }
        step(&mut self.proj_sel, delta, self.projects.len());
        let Some(project_id) = self.projects.get(self.proj_sel).map(|project| project.id) else {
            return Ok(());
        };
        self.select_tree_project(project_id);
        self.focus = Focus::Left;
        Ok(())
    }

    fn move_kanban_lane(&mut self, delta: isize) {
        step(
            &mut self.kanban_lane_sel,
            delta,
            ui::vm::KanbanLane::ALL.len(),
        );
        self.clamp_kanban();
        self.sync_selected_text_scroll();
    }

    fn move_kanban_card(&mut self, delta: isize) {
        let len = self.kanban_items_for_lane(self.kanban_lane_sel).len();
        step(&mut self.kanban_card_sel, delta, len);
        self.sync_selected_text_scroll();
    }

    fn move_issue_section(&mut self, delta: isize) {
        self.issue_section = self.issue_section.step(delta);
        self.issue_section_mode = IssueSectionMode::Selected;
    }

    pub fn selected_issue_section(&self) -> IssueDetailSection {
        self.issue_section
    }

    pub fn issue_section_is_active(&self) -> bool {
        self.issue_section_mode == IssueSectionMode::Active
    }

    fn issue_log_section_active(&self) -> bool {
        self.focus == Focus::IssueDetail
            && self.issue_section == IssueDetailSection::Log
            && self.issue_section_is_active()
    }

    fn activate_issue_section(&mut self) {
        if self.issue_section.is_interactive() {
            self.issue_section_mode = IssueSectionMode::Active;
            self.status = format!("{} active", self.issue_section.title().to_ascii_lowercase());
        } else {
            self.status = format!(
                "{} selected; no direct controls",
                self.issue_section.title().to_ascii_lowercase()
            );
        }
    }

    fn enter_issue_detail(&mut self, return_focus: Focus, return_tree_sel: Option<usize>) {
        self.issue_return_focus = return_focus;
        self.issue_return_tree_sel = return_tree_sel;
        self.focus = Focus::IssueDetail;
        self.issue_section_mode = IssueSectionMode::Selected;
    }

    fn enter_issue_detail_for_issue(
        &mut self,
        issue_id: i64,
        return_focus: Focus,
        return_tree_sel: Option<usize>,
    ) -> bool {
        let Some(issue) = self.issue_by_id(issue_id).cloned() else {
            return false;
        };
        self.detail.issue = Some(issue);
        if let Some(idx) = self.issues().iter().position(|issue| issue.id == issue_id) {
            self.issue_sel = idx;
        }
        self.enter_issue_detail(return_focus, return_tree_sel);
        true
    }

    fn enter_selected_issue_detail_from_left(&mut self) -> bool {
        if self.selected_issue_id().is_none() {
            return false;
        }
        self.enter_issue_detail(Focus::Left, None);
        true
    }

    fn open_context_add_form(&mut self) {
        if !self.capabilities().has(CapabilityAction::Add) {
            self.status = match self.selected_issue() {
                Some(issue) => {
                    format!("issue cannot receive steering in {}", issue.status.as_str())
                }
                None => "nothing addable here".into(),
            };
            return;
        }
        if self.view == View::Config {
            self.form = Some(Form::arsenal_preset(None));
            return;
        }
        if self.focus == Focus::IssueDetail {
            if self.selected_issue_id().is_some() {
                self.form = Some(Form::steering());
            } else {
                self.status = "select an issue first".into();
            }
            return;
        }

        match self.selected_context_item() {
            Some(TreeItem::Project(_)) | None => {
                self.form = Some(self.new_project_form());
            }
            Some(TreeItem::BacklogRoot(_) | TreeItem::Backlog { .. }) => {
                self.form = Some(Form::backlog());
            }
            Some(TreeItem::Issue { .. }) => {
                if self.selected_issue_id().is_some() {
                    self.form = Some(Form::steering());
                } else {
                    self.status = "select an issue first".into();
                }
            }
            Some(TreeItem::RoutinesRoot(_) | TreeItem::Routine { .. }) => {
                self.form = Some(Form::routine());
            }
            Some(TreeItem::ArchivedIssue { .. }) => {
                self.status = "archived issue cannot receive steering".into();
            }
            Some(TreeItem::IssuesRoot(_) | TreeItem::ArchiveRoot(_)) => {
                self.status = "select backlog or issue context first".into();
            }
        }
    }

    async fn move_project_order(&mut self, delta: isize) -> Result<()> {
        let Some(project) = self.projects.get(self.proj_sel).cloned() else {
            return Ok(());
        };
        self.req_ok(
            Command::MoveProjectInProfile {
                project_id: project.id,
                delta,
            },
            "move project",
        )
        .await;
        self.refresh_projects().await?;
        self.select_tree_project(project.id);
        Ok(())
    }

    async fn move_project_profile(&mut self, delta: isize) -> Result<()> {
        let Some(project) = self.projects.get(self.proj_sel).cloned() else {
            return Ok(());
        };
        if self.profiles.is_empty() {
            self.status = "no profiles loaded".into();
            return Ok(());
        }
        let current = self
            .profiles
            .iter()
            .position(|profile| profile.id == project.profile_id)
            .unwrap_or(0);
        let mut next = current;
        step(&mut next, delta, self.profiles.len());
        if next == current {
            return Ok(());
        }
        let target = self.profiles[next].clone();
        self.req_ok(
            Command::MoveProjectToProfile {
                project_id: project.id,
                profile_id: target.id,
            },
            "move project",
        )
        .await;
        self.refresh_projects().await?;
        self.select_tree_project(project.id);
        self.status = format!("moved {} to {}", project.name, target.name);
        Ok(())
    }

    fn selected_move_scope(&self) -> Option<MoveScope> {
        match self.selected_tree_item()? {
            TreeItem::Project(_) => Some(MoveScope::Project),
            TreeItem::Backlog { .. } => Some(MoveScope::Backlog),
            TreeItem::Issue { .. } => Some(MoveScope::Issue),
            _ => None,
        }
    }

    async fn move_selected_item(&mut self, delta: isize) -> Result<()> {
        match self.selected_tree_item() {
            Some(TreeItem::Project(_)) => self.move_project_order(delta).await,
            Some(TreeItem::Backlog { project_id, id }) => {
                self.move_backlog_local(project_id, id, delta);
                Ok(())
            }
            Some(TreeItem::Issue { project_id, id }) => {
                self.move_issue_local(project_id, id, delta);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn move_selected_item_across(&mut self, delta: isize) -> Result<()> {
        if matches!(self.selected_move_scope(), Some(MoveScope::Project)) {
            self.move_project_profile(delta).await
        } else {
            self.status = "h/l only moves projects between profiles".into();
            Ok(())
        }
    }

    fn move_backlog_local(&mut self, project_id: i64, backlog_id: i64, delta: isize) {
        let Some(children) = self.children.get_mut(&project_id) else {
            return;
        };
        let Some(current) = children
            .backlog
            .iter()
            .position(|item| item.id == backlog_id)
        else {
            return;
        };
        let next = (current as isize + delta)
            .clamp(0, children.backlog.len().saturating_sub(1) as isize)
            as usize;
        if next == current {
            return;
        }
        children.backlog.swap(current, next);
        self.select_tree_backlog(backlog_id);
        self.status = "reordered backlog in loaded view".into();
    }

    fn move_issue_local(&mut self, project_id: i64, issue_id: i64, delta: isize) {
        let Some(children) = self.children.get_mut(&project_id) else {
            return;
        };
        let Some(current) = children
            .issues
            .iter()
            .position(|issue| issue.id == issue_id)
        else {
            return;
        };
        let next = (current as isize + delta)
            .clamp(0, children.issues.len().saturating_sub(1) as isize) as usize;
        if next == current {
            return;
        }
        children.issues.swap(current, next);
        self.select_tree_issue(issue_id);
        self.status = "reordered issue in loaded view".into();
    }

    fn select_tree_project(&mut self, project_id: i64) {
        if let Some(idx) = self
            .tree_rows()
            .iter()
            .position(|row| matches!(&row.item, TreeItem::Project(id) if *id == project_id))
        {
            self.tree_sel = idx;
        }
        self.sync_active_project();
    }

    fn select_tree_backlog(&mut self, backlog_id: i64) {
        if let Some(idx) = self
            .tree_rows()
            .iter()
            .position(|row| matches!(&row.item, TreeItem::Backlog { id, .. } if *id == backlog_id))
        {
            self.tree_sel = idx;
        }
        self.sync_active_project();
    }

    fn select_tree_issue(&mut self, issue_id: i64) {
        if let Some(idx) = self
            .tree_rows()
            .iter()
            .position(|row| matches!(&row.item, TreeItem::Issue { id, .. } if *id == issue_id))
        {
            self.tree_sel = idx;
        }
        self.sync_active_project();
    }

    fn preserve_tree_issue_selection(&mut self, project_id: i64, issue_id: i64) -> bool {
        let Some(kids) = self.children.get(&project_id) else {
            return false;
        };
        let is_active = kids.issues.iter().position(|issue| issue.id == issue_id);
        let is_archived = kids
            .archived_issues
            .iter()
            .position(|issue| issue.id == issue_id);
        if is_active.is_none() && is_archived.is_none() {
            return false;
        }

        self.expanded.insert(project_id);
        if let Some(idx) = is_active {
            self.issue_sel = idx;
        }
        if is_archived.is_some() {
            self.archive_expanded.insert(project_id);
            if let Some(idx) = self.tree_rows().iter().position(|row| {
                matches!(
                    &row.item,
                    TreeItem::ArchivedIssue {
                        project_id: pid,
                        id
                    } if *pid == project_id && *id == issue_id
                )
            }) {
                self.tree_sel = idx;
                self.sync_active_project();
                return true;
            }
        }
        if let Some(idx) = self.tree_rows().iter().position(|row| {
            matches!(
                &row.item,
                TreeItem::Issue {
                    project_id: pid,
                    id
                } if *pid == project_id && *id == issue_id
            )
        }) {
            self.tree_sel = idx;
            self.sync_active_project();
            return true;
        }
        false
    }

    fn selected_issue_matches(&self, issue_id: i64) -> bool {
        self.selected_issue_id() == Some(issue_id)
    }

    fn clamp_tree(&mut self) {
        let len = self.tree_rows().len();
        step(&mut self.tree_sel, 0, len);
    }

    async fn drill(&mut self) -> Result<()> {
        if self.view != View::Overview {
            return Ok(());
        }
        if self.focus == Focus::ProjectKanban {
            self.open_selected_kanban_item().await?;
            return Ok(());
        }
        match self.selected_tree_item() {
            // Enter on a project header moves focus to the right-side kanban.
            Some(TreeItem::Project(pid)) => {
                self.expanded.insert(pid);
                self.clamp_tree();
                self.sync_active_project();
                self.focus = Focus::ProjectKanban;
                self.clamp_kanban();
            }
            Some(
                TreeItem::RoutinesRoot(pid)
                | TreeItem::BacklogRoot(pid)
                | TreeItem::IssuesRoot(pid),
            ) => {
                if self.expanded.contains(&pid) {
                    self.expanded.remove(&pid);
                } else {
                    self.expanded.insert(pid);
                }
                self.clamp_tree();
                self.sync_active_project();
            }
            Some(TreeItem::ArchiveRoot(pid)) => {
                if self.archive_expanded.contains(&pid) {
                    self.archive_expanded.remove(&pid);
                } else {
                    self.archive_expanded.insert(pid);
                }
                self.clamp_tree();
                self.sync_active_project();
            }
            // Enter on an issue keeps the main screen and moves focus to the detail pane.
            Some(TreeItem::Issue { .. } | TreeItem::ArchivedIssue { .. }) => {
                if self.enter_selected_issue_detail_from_left() {
                    self.refresh_detail().await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn open_selected_kanban_item(&mut self) -> Result<()> {
        match self.selected_kanban_item() {
            Some(ui::vm::KanbanItem::Backlog(id)) => {
                self.select_tree_backlog(id);
                self.focus = Focus::Left;
            }
            Some(ui::vm::KanbanItem::Issue(id)) => {
                let return_tree_sel = Some(self.tree_sel);
                if !self.enter_issue_detail_for_issue(id, Focus::ProjectKanban, return_tree_sel) {
                    self.status = "kanban issue is no longer loaded".into();
                    return Ok(());
                }
                self.refresh_detail().await?;
            }
            None => self.status = "kanban lane is empty".into(),
        }
        Ok(())
    }

    // --- live events --------------------------------------------------------

    fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    /// React to a daemon event: log it, then re-query whatever it touched.
    async fn on_event(&mut self, ev: Event) -> Result<()> {
        self.push_log(format_event(&ev));
        match ev {
            Event::IssueStatus { issue_id, .. } => {
                let should_reselect_issue = self.selected_issue_row_id() == Some(issue_id);
                self.refresh_issues().await?;
                if should_reselect_issue {
                    self.select_issue_in_tree(issue_id);
                }
                self.refresh_detail().await?;
                self.refresh_activity().await?;
            }
            Event::IssueRemoved { .. }
            | Event::FindingAdded { .. }
            | Event::SteeringAdded { .. } => {
                self.refresh_issues().await?;
                self.refresh_detail().await?;
                self.refresh_activity().await?;
            }
            Event::IssueLog { issue_id, .. } => {
                if self.selected_issue_matches(issue_id) {
                    self.refresh_issue_runs_and_tail().await?;
                }
            }
            Event::BacklogChanged { .. } => {
                self.refresh_backlog().await?;
                self.refresh_activity().await?;
            }
            Event::AskAnswered { project_id, .. } => {
                if Some(project_id) == self.selected_project_id() {
                    self.refresh_asks().await?;
                }
            }
            Event::SchedulerTick { project_id } => {
                self.refresh_last_auto_tick(project_id).await?;
                self.refresh_project_children(project_id).await?;
                if Some(project_id) == self.selected_project_id() {
                    self.refresh_activity().await?;
                }
            }
            Event::MainJobStatus { .. } | Event::RoutineFired { .. } => {
                self.refresh_routines().await?;
                self.refresh_activity().await?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Move a selection index by `delta`, clamped to `[0, len)`. No-op on empty.
fn step(sel: &mut usize, delta: isize, len: usize) {
    if len == 0 {
        *sel = 0;
        return;
    }
    let max = len as isize - 1;
    *sel = (*sel as isize + delta).clamp(0, max) as usize;
}

/// One-line human form of an event for the Logs view.
fn format_event(ev: &Event) -> String {
    match ev {
        Event::IssueStatus { issue_id, status } => {
            format!("issue #{issue_id} → {}", status.as_str())
        }
        Event::IssueRemoved { issue_id, .. } => format!("issue #{issue_id} removed"),
        Event::AskAnswered {
            answer_id,
            project_id,
        } => {
            format!("ask answer #{answer_id} on project #{project_id}")
        }
        Event::IssueLog {
            issue_id, phase, ..
        } => format!("issue #{issue_id} log ({phase})"),
        Event::BacklogChanged {
            item_id, approval, ..
        } => {
            format!("backlog #{item_id} → {approval}")
        }
        Event::SchedulerTick { project_id } => format!("scheduler tick project #{project_id}"),
        Event::FindingAdded {
            finding_id,
            issue_id,
        } => {
            format!("finding #{finding_id} on issue #{issue_id}")
        }
        Event::SteeringAdded {
            steering_id,
            issue_id,
        } => {
            format!("queue message #{steering_id} on issue #{issue_id}")
        }
        Event::MainJobStatus {
            main_job_id,
            status,
        } => {
            format!("main job #{main_job_id} → {status:?}")
        }
        Event::RoutineFired {
            routine_id,
            main_job_id,
        } => {
            format!("routine #{routine_id} fired → main job #{main_job_id}")
        }
        Event::DaemonLifecycle { kind } => format!("daemon {kind}"),
    }
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Enter raw mode + the alternate screen and build the ratatui terminal.
fn setup_terminal() -> Result<Tui> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen).context("entering alternate screen")?;
    Terminal::new(CrosstermBackend::new(out)).context("building terminal")
}

/// Best-effort restore: leave the alternate screen and raw mode. Safe to call
/// more than once (idempotent enough for the panic hook + normal exit path).
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
}

/// Run the TUI against the daemon at `socket` until the user quits. Requires the
/// daemon to be running; if the initial connection fails the terminal is never
/// touched and the error propagates with a hint.
pub async fn run(socket: PathBuf) -> Result<()> {
    let mut app = App::new(socket.clone());

    // Fail fast (before touching the terminal) if the daemon is down.
    app.refresh_all().await.with_context(|| {
        format!(
            "cannot reach daemon at {} (is `auwsx daemon` running?)",
            socket.display()
        )
    })?;

    // Restore the terminal even on panic, so a crash never leaves a wrecked tty.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut app, &socket).await;
    restore_terminal();
    let _ = terminal.show_cursor();
    result
}

async fn event_loop(terminal: &mut Tui, app: &mut App, socket: &Path) -> Result<()> {
    // Blocking key reader on its own OS thread → async channel. crossterm's
    // event::read is blocking; this keeps it off the runtime without needing
    // the optional event-stream feature.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<CEvent>();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            let keep = matches!(
                ev,
                CEvent::Key(KeyEvent {
                    kind: KeyEventKind::Press,
                    ..
                }) | CEvent::Resize(_, _)
            );
            if keep && event_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut events = match ipc::EventStream::connect(socket).await {
        Ok(es) => {
            app.connected = true;
            Some(es)
        }
        Err(_) => None,
    };

    // One-shot background git-repo scan for the New-project form's completion.
    // `spawn_blocking` keeps the filesystem walk off the async runtime; the
    // result lands once via the select! arm below, then `repo_scan` is None.
    let mut repo_scan = Some(tokio::task::spawn_blocking(
        crate::repo_scan::scan_git_repos,
    ));

    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        if app.needs_redraw {
            let force = app.force_redraw;
            app.force_redraw = false;
            draw_sync(terminal, force, |f| ui::draw(f, app))?;
            app.needs_redraw = false;
        }

        tokio::select! {
            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else { break }; // reader thread gone
                match event {
                    CEvent::Resize(_, _) => {
                        app.request_force_redraw();
                    }
                    CEvent::Key(key) => {
                        if app.confirm_quit {
                            let before = app.render_revision();
                            if app.handle_confirm_key(key).await? {
                                break;
                            }
                            if app.render_revision() != before {
                                app.request_redraw();
                            }
                        } else if app.form.is_some() {
                            let before = app.render_revision();
                            app.handle_form_key(key).await?;
                            if app.render_revision() != before {
                                app.request_redraw();
                            }
                        } else if let Some(action) = input::map_key(app.view, key) {
                            let before = app.render_revision();
                            if app.apply(action).await? {
                                break;
                            }
                            if app.render_revision() != before {
                                app.request_redraw();
                            }
                        }
                    }
                    _ => {}
                }
            }
            ev = next_event(&mut events) => match ev {
                Some(Ok(e)) => {
                    app.on_event(e).await?;
                    app.request_redraw();
                }
                _ => {
                    // Stream closed or errored: drop to poll-only mode.
                    events = None;
                    app.connected = false;
                    app.request_redraw();
                }
            },
            repos = drain_scan(&mut repo_scan) => {
                app.scanned_repos = repos;
                repo_scan = None; // consumed; never poll the finished handle again
                app.request_redraw();
            }
            _ = tick.tick() => {
                if app.ui_tick().await? {
                    app.request_redraw();
                }
            }
        }
    }
    Ok(())
}

fn draw_sync<F>(terminal: &mut Tui, clear: bool, f: F) -> Result<()>
where
    F: FnOnce(&mut ratatui::Frame),
{
    execute!(std::io::stdout(), BeginSynchronizedUpdate)
        .context("beginning synchronized terminal update")?;
    let result = (|| -> Result<()> {
        if clear {
            terminal.clear()?;
        }
        terminal.draw(f)?;
        Ok(())
    })();
    let end = execute!(std::io::stdout(), EndSynchronizedUpdate)
        .context("ending synchronized terminal update");
    result.and(end)
}

/// Await the repo-scan task if one is pending; an absent/finished handle parks
/// forever so `select!` falls through to the other arms.
async fn drain_scan(handle: &mut Option<tokio::task::JoinHandle<Vec<String>>>) -> Vec<String> {
    match handle {
        Some(h) => h.await.unwrap_or_default(),
        None => std::future::pending().await,
    }
}

/// Await the next event from an optional stream; an absent stream parks forever
/// (so `select!` falls through to keys + tick). `Some(Err)`/`None` => stream gone.
async fn next_event(es: &mut Option<ipc::EventStream>) -> Option<Result<Event>> {
    match es {
        Some(s) => match s.next().await {
            Ok(Some(e)) => Some(Ok(e)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        },
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auwsx_core::db::agent_runs::Role;
    use auwsx_core::state::IssueStatus;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[test]
    fn view_step_forward_one() {
        assert_eq!(View::Overview.step(1), View::Backlog);
    }

    #[test]
    fn view_step_wraps_forward() {
        assert_eq!(View::Ask.step(1), View::Overview);
    }

    #[test]
    fn view_step_wraps_backward() {
        assert_eq!(View::Overview.step(-1), View::Ask);
    }

    #[test]
    fn view_step_skips_config() {
        assert_eq!(View::Backlog.step(2), View::Ask);
    }

    #[tokio::test]
    async fn given_issue_selected_when_drill_then_focuses_detail_without_fullscreen(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Issue { id: 7, .. }))
            .expect("issue row exists");
        app.sync_active_project();

        assert!(app.enter_selected_issue_detail_from_left());

        assert_eq!(app.view, View::Overview);
        assert_eq!(app.focus, Focus::IssueDetail);
        assert_eq!(app.issue_return_focus, Focus::Left);
        assert_eq!(app.issue_section_mode, IssueSectionMode::Selected);
        Ok(())
    }

    #[tokio::test]
    async fn given_issue_log_section_inactive_when_jk_then_moves_section_not_log(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        app.view = View::Overview;
        app.focus = Focus::IssueDetail;
        app.issue_section = IssueDetailSection::Log;
        app.log_tail = "one\ntwo\nthree\n".to_string();

        app.apply(Action::Up).await?;

        assert_eq!(app.selected_issue_section(), IssueDetailSection::WorkQueue);
        assert_eq!(app.issue_log_scroll, 0);
        assert_eq!(app.issue_section_mode, IssueSectionMode::Selected);
        Ok(())
    }

    #[tokio::test]
    async fn given_issue_log_section_entered_when_jk_then_scrolls_log() -> anyhow::Result<()> {
        let mut app = test_app();
        app.view = View::Overview;
        app.focus = Focus::IssueDetail;
        app.issue_section = IssueDetailSection::Log;
        app.log_tail = "one\ntwo\nthree\n".to_string();

        app.apply(Action::Drill).await?;
        app.apply(Action::Up).await?;

        assert_eq!(app.selected_issue_section(), IssueDetailSection::Log);
        assert_eq!(app.issue_log_scroll, 1);
        assert_eq!(app.issue_section_mode, IssueSectionMode::Active);
        Ok(())
    }

    #[tokio::test]
    async fn given_non_log_issue_section_entered_then_selection_stays_noninteractive(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        app.view = View::Overview;
        app.focus = Focus::IssueDetail;
        app.issue_section = IssueDetailSection::Findings;

        app.apply(Action::Drill).await?;

        assert_eq!(app.selected_issue_section(), IssueDetailSection::Findings);
        assert_eq!(app.issue_section_mode, IssueSectionMode::Selected);
        assert_eq!(app.status, "findings selected; no direct controls");
        Ok(())
    }

    #[tokio::test]
    async fn given_issue_log_section_active_when_esc_then_exits_section_depth() -> anyhow::Result<()>
    {
        let mut app = test_app();
        app.view = View::Overview;
        app.focus = Focus::IssueDetail;
        app.issue_section = IssueDetailSection::Log;
        app.issue_section_mode = IssueSectionMode::Active;

        app.apply(Action::Back).await?;

        assert_eq!(app.focus, Focus::IssueDetail);
        assert_eq!(app.issue_section_mode, IssueSectionMode::Selected);
        Ok(())
    }

    #[test]
    fn given_issue_tree_row_when_rendered_then_status_is_compact_without_big_state_label() {
        let mut app = test_app();
        let mut issue = issue_fixture();
        issue.status = auwsx_core::state::IssueStatus::PlanBlocked;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);

        let label = app
            .tree_rows()
            .into_iter()
            .find(|row| matches!(row.item, TreeItem::Issue { id: 7, .. }))
            .expect("issue row exists")
            .label;

        assert!(label.starts_with("! PLAN #7"));
        for duplicate in ["Need Att", "Active", "Idle", "Done"] {
            assert!(
                !label.contains(duplicate),
                "{duplicate} leaked into {label}"
            );
        }
    }

    #[test]
    fn given_issue_tree_row_with_description_when_rendered_then_description_is_visible() {
        let mut app = test_app();
        let mut issue = issue_fixture();
        issue.title = "cursor".to_string();
        issue.description = Some("left/right movement in input fields".to_string());
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);

        let label = app
            .tree_rows()
            .into_iter()
            .find(|row| matches!(row.item, TreeItem::Issue { id: 7, .. }))
            .expect("issue row exists")
            .label;

        assert_eq!(
            label,
            "◉ PLAN #7   cursor - left/right movement in input fields  active"
        );
    }

    #[test]
    fn given_multiline_backlog_selected_when_text_scroll_advances_then_offset_moves() {
        let mut app = test_app();
        let mut backlog = backlog_fixture();
        backlog.text = "one\ntwo\nthree".to_string();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                backlog: vec![backlog],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Backlog { id: 1, .. }))
            .expect("backlog row exists");

        app.sync_selected_text_scroll();
        assert_eq!(app.selected_text_scroll_offset("backlog:1", 3), 0);

        app.advance_selected_text_scroll();

        assert_eq!(app.selected_text_scroll_offset("backlog:1", 3), 1);
    }

    #[test]
    fn given_selected_text_changes_when_synced_then_scroll_resets() {
        let mut app = test_app();
        let mut first = backlog_fixture();
        first.id = 1;
        first.text = "one\ntwo".to_string();
        let mut second = backlog_fixture();
        second.id = 2;
        second.text = "alpha\nbeta".to_string();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                backlog: vec![first, second],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Backlog { id: 1, .. }))
            .expect("first backlog row exists");
        app.sync_selected_text_scroll();
        app.advance_selected_text_scroll();
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Backlog { id: 2, .. }))
            .expect("second backlog row exists");

        app.sync_selected_text_scroll();

        assert_eq!(app.selected_text_scroll_key.as_deref(), Some("backlog:2"));
        assert_eq!(app.selected_text_scroll_offset, 0);
    }

    #[test]
    fn given_backlog_item_selected_when_capabilities_requested_then_move_visible() {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                backlog: vec![backlog_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Backlog { id: 1, .. }))
            .expect("backlog row exists");

        assert!(app.capabilities().has(CapabilityAction::MoveMode));
    }

    #[test]
    fn given_issue_item_selected_when_capabilities_requested_then_move_visible() {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Issue { id: 7, .. }))
            .expect("issue row exists");

        assert!(app.capabilities().has(CapabilityAction::MoveMode));
    }

    #[tokio::test]
    async fn given_backlog_move_mode_when_down_then_loaded_backlog_order_swaps(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        let mut first = backlog_fixture();
        first.id = 1;
        let mut second = backlog_fixture();
        second.id = 2;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                backlog: vec![first, second],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Backlog { id: 1, .. }))
            .expect("first backlog row exists");

        app.apply(Action::MoveMode).await?;
        app.apply(Action::Down).await?;

        let ids: Vec<i64> = app
            .children
            .get(&1)
            .unwrap()
            .backlog
            .iter()
            .map(|b| b.id)
            .collect();
        assert_eq!(ids, vec![2, 1]);
        assert!(matches!(
            app.selected_tree_item(),
            Some(TreeItem::Backlog { id: 1, .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn given_move_mode_active_when_move_key_pressed_then_exits() -> anyhow::Result<()> {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.tree_sel = 0;

        app.apply(Action::MoveMode).await?;
        app.apply(Action::MoveMode).await?;

        assert!(!app.move_mode);
        assert_eq!(app.status, "move mode off");
        Ok(())
    }

    #[tokio::test]
    async fn given_issue_move_mode_when_down_then_loaded_issue_order_swaps() -> anyhow::Result<()> {
        let mut app = test_app();
        let mut first = issue_fixture();
        first.id = 7;
        let mut second = issue_fixture();
        second.id = 8;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![first, second],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Issue { id: 7, .. }))
            .expect("first issue row exists");

        app.apply(Action::MoveMode).await?;
        app.apply(Action::Down).await?;

        let ids: Vec<i64> = app
            .children
            .get(&1)
            .unwrap()
            .issues
            .iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, vec![8, 7]);
        assert!(matches!(
            app.selected_tree_item(),
            Some(TreeItem::Issue { id: 7, .. })
        ));
        Ok(())
    }

    #[test]
    fn given_project_row_selected_when_issue_sel_points_elsewhere_then_no_issue_is_selected() {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| row.item == TreeItem::Project(1))
            .expect("project row exists");
        app.issue_sel = 0;

        assert_eq!(app.selected_issue_id(), None);
        assert!(app.selected_issue().is_none());
    }

    #[tokio::test]
    async fn given_backlog_item_when_add_then_opens_backlog_form() -> anyhow::Result<()> {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                backlog: vec![backlog_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Backlog { id: 1, .. }))
            .expect("backlog row exists");

        app.apply(Action::Add).await?;

        assert!(matches!(
            app.form.as_ref().map(|form| &form.kind),
            Some(FormKind::Backlog)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn given_issue_item_when_add_then_opens_steering_form() -> anyhow::Result<()> {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Issue { id: 7, .. }))
            .expect("issue row exists");

        app.apply(Action::Add).await?;

        assert!(matches!(
            app.form.as_ref().map(|form| &form.kind),
            Some(FormKind::QueueMessage)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn given_kanban_backlog_when_add_then_opens_backlog_form() -> anyhow::Result<()> {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                backlog: vec![backlog_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.focus = Focus::ProjectKanban;
        app.kanban_lane_sel = 0;
        app.kanban_card_sel = 0;

        let hints = app.capabilities();
        assert!(hints.has(CapabilityAction::Drill));
        assert!(hints.has(CapabilityAction::Add));
        assert!(hints
            .hints
            .iter()
            .any(|hint| hint.label == "(Enter) select"));
        assert!(hints.hints.iter().any(|hint| hint.label == "(a)dd backlog"));

        app.apply(Action::Add).await?;

        assert!(matches!(
            app.form.as_ref().map(|form| &form.kind),
            Some(FormKind::Backlog)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn given_kanban_issue_when_add_then_opens_steering_form() -> anyhow::Result<()> {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.focus = Focus::ProjectKanban;
        app.kanban_lane_sel = 0;
        app.kanban_card_sel = 0;

        let hints = app.capabilities();
        assert!(hints.has(CapabilityAction::Drill));
        assert!(hints.has(CapabilityAction::Add));
        assert!(hints
            .hints
            .iter()
            .any(|hint| hint.label == "(Enter) detail"));
        assert!(hints.hints.iter().any(|hint| hint.label == "(a)steer"));

        app.apply(Action::Add).await?;

        assert!(matches!(
            app.form.as_ref().map(|form| &form.kind),
            Some(FormKind::QueueMessage)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn given_issue_detail_when_add_then_opens_steering_form() -> anyhow::Result<()> {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.detail.issue = Some(issue_fixture());
        app.focus = Focus::IssueDetail;

        let hints = app.capabilities();
        assert!(hints.has(CapabilityAction::Add));
        assert!(hints.hints.iter().any(|hint| hint.label == "(a)steer"));

        app.apply(Action::Add).await?;

        assert!(matches!(
            app.form.as_ref().map(|form| &form.kind),
            Some(FormKind::QueueMessage)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn given_backlog_item_when_capabilities_requested_then_approve_uses_capital_a() {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                backlog: vec![backlog_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Backlog { id: 1, .. }))
            .expect("backlog row exists");

        let hints = app.capabilities();

        assert!(hints.has(CapabilityAction::Approve));
        assert!(hints.hints.iter().any(|hint| hint.label == "(A)pprove"));
    }

    #[tokio::test]
    async fn given_bracket_project_jump_when_applied_then_selects_adjacent_project(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        let mut first = project_fixture();
        first.id = 1;
        first.name = "first".into();
        let mut second = project_fixture();
        second.id = 2;
        second.name = "second".into();
        app.projects = vec![first, second];

        app.apply(Action::NextProject).await?;

        assert_eq!(app.proj_sel, 1);
        assert_eq!(app.selected_tree_item(), Some(TreeItem::Project(2)));
        Ok(())
    }

    #[test]
    fn given_full_issue_view_when_no_issue_row_selected_then_issue_sel_remains_legacy_selection() {
        let mut app = test_app();
        app.view = View::Issue;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.tree_sel = 0;
        app.issue_sel = 0;

        assert_eq!(app.selected_issue_id(), Some(7));
        assert_eq!(app.selected_issue().map(|issue| issue.id), Some(7));
    }

    #[test]
    fn given_archived_issue_when_tree_rendered_then_hidden_behind_archive_section() {
        let mut app = test_app();
        let mut active = issue_fixture();
        active.id = 7;
        active.status = auwsx_core::state::IssueStatus::Working;
        let mut archived = issue_fixture();
        archived.id = 8;
        archived.status = auwsx_core::state::IssueStatus::Done;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![active],
                archived_issues: vec![archived],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);

        let rows = app.tree_rows();

        assert!(rows
            .iter()
            .any(|row| matches!(row.item, TreeItem::Issue { id: 7, .. })));
        assert!(!rows
            .iter()
            .any(|row| matches!(row.item, TreeItem::Issue { id: 8, .. })));
    }

    #[test]
    fn given_only_archived_issue_when_tree_rendered_then_left_nav_has_no_archive_rows() {
        let mut app = test_app();
        let mut archived = issue_fixture();
        archived.id = 8;
        archived.status = auwsx_core::state::IssueStatus::Done;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                archived_issues: vec![archived],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);

        let rows = app.tree_rows();

        assert!(!rows
            .iter()
            .any(|row| matches!(row.item, TreeItem::Issue { id: 8, .. })));
        assert!(rows.iter().any(|row| row.item == TreeItem::Project(1)));
    }

    #[test]
    fn given_selected_issue_becomes_archived_when_refresh_preserves_selection_then_project_is_selected(
    ) {
        let mut app = test_app();
        let mut archived = issue_fixture();
        archived.id = 8;
        archived.status = auwsx_core::state::IssueStatus::Done;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                archived_issues: vec![archived],
                ..ProjectChildren::default()
            },
        );

        assert!(app.preserve_tree_issue_selection(1, 8));

        assert!(app.expanded.contains(&1));
        assert!(app.archive_expanded.contains(&1));
        assert_eq!(app.selected_issue_row_id(), Some(8));
        assert_eq!(
            app.selected_tree_item(),
            Some(TreeItem::ArchivedIssue {
                project_id: 1,
                id: 8
            })
        );
    }

    #[test]
    fn given_done_issue_when_delete_action_requested_then_archives_not_abandons() {
        let mut app = test_app();
        let mut issue = issue_fixture();
        issue.status = auwsx_core::state::IssueStatus::Done;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.select_tree_issue(7);

        let (cmd, label) = app.issue_delete_command(7);

        assert_eq!(label, "archive issue");
        assert_eq!(app.selected_issue_delete_hint(), "d archive");
        assert!(matches!(cmd, Command::CleanupIssueWorktree { issue_id: 7 }));
    }

    #[test]
    fn given_archived_issue_selected_when_delete_action_requested_then_keeps_archive_semantics() {
        let mut app = test_app();
        let mut issue = issue_fixture();
        issue.status = auwsx_core::state::IssueStatus::Done;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                archived_issues: vec![issue],
                ..ProjectChildren::default()
            },
        );
        app.focus = Focus::IssueDetail;
        app.detail.issue = app.archived_issues().first().cloned();

        let (cmd, label) = app.issue_delete_command(7);

        assert_eq!(label, "archive issue");
        assert_eq!(app.selected_issue_delete_hint(), "d archive");
        assert!(matches!(cmd, Command::CleanupIssueWorktree { issue_id: 7 }));
    }

    #[test]
    fn given_selection_differs_from_target_when_delete_action_requested_then_target_status_wins() {
        let mut app = test_app();
        let mut active = issue_fixture();
        active.id = 7;
        active.status = auwsx_core::state::IssueStatus::Working;
        let mut archived = issue_fixture();
        archived.id = 8;
        archived.status = auwsx_core::state::IssueStatus::Done;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![active],
                archived_issues: vec![archived],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.select_tree_issue(7);

        let (cmd, label) = app.issue_delete_command(8);

        assert_eq!(label, "archive issue");
        assert!(matches!(cmd, Command::CleanupIssueWorktree { issue_id: 8 }));
    }

    #[test]
    fn given_working_issue_when_delete_action_requested_then_abandons() {
        let mut app = test_app();
        let mut issue = issue_fixture();
        issue.status = auwsx_core::state::IssueStatus::Working;
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue],
                ..ProjectChildren::default()
            },
        );
        app.expanded.insert(1);
        app.select_tree_issue(7);

        let (cmd, label) = app.issue_delete_command(7);

        assert_eq!(label, "abandon issue");
        assert_eq!(app.selected_issue_delete_hint(), "d abandon");
        assert!(matches!(cmd, Command::AbandonIssue { issue_id: 7 }));
    }

    #[test]
    fn free_step_increments() {
        let mut sel = 0usize;
        step(&mut sel, 1, 3);
        assert_eq!(sel, 1);
    }

    #[test]
    fn free_step_clamps_at_top() {
        let mut sel = 2usize;
        step(&mut sel, 1, 3);
        assert_eq!(sel, 2);
    }

    #[test]
    fn free_step_clamps_at_bottom() {
        let mut sel = 0usize;
        step(&mut sel, -1, 3);
        assert_eq!(sel, 0);
    }

    #[test]
    fn free_step_clamps_stale_index() {
        let mut sel = 5usize;
        step(&mut sel, 0, 3);
        assert_eq!(sel, 2);
    }

    #[test]
    fn free_step_empty_list_resets_to_zero() {
        let mut sel = 0usize;
        step(&mut sel, 1, 0);
        assert_eq!(sel, 0);
    }

    #[test]
    fn given_issue_log_when_scroll_up_then_moves_older() {
        let mut app = test_app();
        app.log_tail = "one\ntwo\nthree\n".to_string();

        app.scroll_issue_log(1);

        assert_eq!(app.issue_log_scroll, 1);
    }

    fn test_agent_run(id: i64, issue_id: i64, log_path: Option<&str>) -> AgentRun {
        AgentRun {
            id,
            issue_id: Some(issue_id),
            main_job_id: None,
            role: Role::Work,
            phase: "fix".into(),
            agent_cmd: "agent".into(),
            status_before: Some("implementing".into()),
            status_after: None,
            pid: Some(123),
            exit_code: None,
            exit_kind: None,
            prompt_path: None,
            log_path: log_path.map(str::to_string),
            spawned_at: 1,
            exited_at: None,
            note: None,
            phase_report: None,
        }
    }

    fn issue_log_socket_path(name: &str) -> PathBuf {
        let dir = PathBuf::from("target/auwsx-tui-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}-{}.sock", std::process::id()))
    }

    async fn serve_selected_issue_log(socket: PathBuf, ready: tokio::sync::oneshot::Sender<()>) {
        if socket.exists() {
            std::fs::remove_file(&socket).unwrap();
        }
        let listener = UnixListener::bind(&socket).unwrap();
        let _ = ready.send(());
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let cmd: Command = serde_json::from_str(&line).unwrap();
            let resp = match cmd {
                Command::ListAgentRunsByIssue { issue_id: 7 } => {
                    Response::AgentRuns(vec![test_agent_run(99, 7, Some("target/agent.log"))])
                }
                Command::TailAgentRunLog {
                    agent_run_id: 99, ..
                } => Response::LogTail {
                    path: "target/agent.log".into(),
                    text: "fresh\nlog".into(),
                },
                other => panic!("unexpected command: {other:?}"),
            };
            let mut stream = reader.into_inner();
            let mut data = serde_json::to_vec(&resp).unwrap();
            data.push(b'\n');
            stream.write_all(&data).await.unwrap();
            stream.flush().await.unwrap();
        }
        std::fs::remove_file(&socket).unwrap();
    }

    #[test]
    fn given_issue_log_at_oldest_when_scroll_older_then_clamps() {
        let mut app = test_app();
        app.log_tail = "one\ntwo\nthree\n".to_string();

        app.scroll_issue_log(10);

        assert_eq!(app.issue_log_scroll, 2);
    }

    #[test]
    fn given_terminal_and_active_issues_when_tree_rows_then_archive_is_collapsed_by_default() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                issues: vec![test_issue(1, IssueStatus::Working, "active")],
                archived_issues: vec![test_issue(2, IssueStatus::Done, "archived")],
                ..ProjectChildren::default()
            },
        );

        let rows = app.tree_rows();
        let labels = rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>();

        assert!(labels.contains(&"Issues    1"));
        assert!(labels.contains(&"Archive   1"));
        assert!(rows.iter().any(|r| r.item
            == TreeItem::Issue {
                project_id: 42,
                id: 1
            }));
        assert!(!rows.iter().any(|r| r.item
            == TreeItem::ArchivedIssue {
                project_id: 42,
                id: 2
            }));
    }

    #[test]
    fn given_expanded_archive_when_tree_rows_then_archived_issues_are_visible() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.archive_expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                issues: vec![test_issue(1, IssueStatus::Working, "active")],
                archived_issues: vec![test_issue(2, IssueStatus::Done, "archived")],
                ..ProjectChildren::default()
            },
        );

        let rows = app.tree_rows();

        assert!(rows.iter().any(|r| r.item
            == TreeItem::Issue {
                project_id: 42,
                id: 1
            }));
        assert!(rows.iter().any(|r| r.item
            == TreeItem::ArchivedIssue {
                project_id: 42,
                id: 2
            }));
    }

    #[test]
    fn given_archived_issue_row_when_selected_then_issue_detail_selection_reuses_issue() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.archive_expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                archived_issues: vec![test_issue(2, IssueStatus::Done, "archived")],
                ..ProjectChildren::default()
            },
        );
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|r| matches!(r.item, TreeItem::ArchivedIssue { id: 2, .. }))
            .unwrap();

        assert_eq!(app.selected_issue_id(), Some(2));
        assert_eq!(
            app.selected_issue().map(|issue| issue.title.as_str()),
            Some("archived")
        );
    }

    #[tokio::test]
    async fn given_archived_issue_row_when_new_context_then_action_is_read_only() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.archive_expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                archived_issues: vec![test_issue(2, IssueStatus::Done, "archived")],
                ..ProjectChildren::default()
            },
        );
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|r| matches!(r.item, TreeItem::ArchivedIssue { id: 2, .. }))
            .unwrap();

        app.apply(Action::Add).await.unwrap();

        assert!(app.form.is_none());
        assert_eq!(app.status, "issue cannot receive steering in DONE");
    }

    #[tokio::test]
    async fn given_queue_message_form_when_selection_becomes_archived_then_submit_is_blocked() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.archive_expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                archived_issues: vec![test_issue(2, IssueStatus::Done, "archived")],
                ..ProjectChildren::default()
            },
        );
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|r| matches!(r.item, TreeItem::ArchivedIssue { id: 2, .. }))
            .unwrap();
        let mut form = Form::steering();
        form.set("note", "do it");
        app.form = Some(form);

        app.submit_form().await.unwrap();

        assert_eq!(app.status, "select an active issue first");
    }

    #[test]
    fn given_non_issue_row_when_issue_selection_falls_back_then_issue_row_id_is_empty() {
        let mut app = test_app();
        app.view = View::Issue;
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                issues: vec![test_issue(7, IssueStatus::Working, "active")],
                ..ProjectChildren::default()
            },
        );
        app.issue_sel = 0;
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|r| matches!(r.item, TreeItem::Project(42)))
            .unwrap();

        assert_eq!(app.selected_issue_id(), Some(7));
        assert_eq!(app.selected_issue_row_id(), None);
    }

    #[tokio::test]
    async fn given_archive_root_when_drilled_then_archived_issue_rows_toggle() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                archived_issues: vec![test_issue(2, IssueStatus::Done, "archived")],
                ..ProjectChildren::default()
            },
        );
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|r| matches!(r.item, TreeItem::ArchiveRoot(42)))
            .unwrap();

        app.drill().await.unwrap();

        assert!(app.archive_expanded.contains(&42));
        assert!(app.tree_rows().iter().any(|r| matches!(
            r.item,
            TreeItem::ArchivedIssue {
                project_id: 42,
                id: 2
            }
        )));

        app.drill().await.unwrap();

        assert!(!app.archive_expanded.contains(&42));
        assert!(!app.tree_rows().iter().any(|r| matches!(
            r.item,
            TreeItem::ArchivedIssue {
                project_id: 42,
                id: 2
            }
        )));
    }

    #[test]
    fn given_archive_root_when_capabilities_requested_then_access_hint_is_explicit() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                archived_issues: vec![test_issue(2, IssueStatus::Done, "archived")],
                ..ProjectChildren::default()
            },
        );
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|r| matches!(r.item, TreeItem::ArchiveRoot(42)))
            .unwrap();

        let closed_hints = app.capabilities();

        assert!(closed_hints
            .hints
            .iter()
            .any(|hint| hint.label == "(Enter) open archive"));

        app.archive_expanded.insert(42);
        let open_hints = app.capabilities();

        assert!(open_hints
            .hints
            .iter()
            .any(|hint| hint.label == "(Enter) close archive"));
    }

    #[test]
    fn given_issue_moves_to_archive_when_reselected_then_tree_focus_follows_issue() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                archived_issues: vec![test_issue(7, IssueStatus::Done, "archived")],
                ..ProjectChildren::default()
            },
        );

        assert!(app.select_issue_in_tree(7));

        assert!(app.archive_expanded.contains(&42));
        assert!(matches!(
            app.selected_tree_item(),
            Some(TreeItem::ArchivedIssue { id: 7, .. })
        ));
        assert!(app.archive_expanded.contains(&42));
        assert_eq!(app.selected_issue_id(), Some(7));
        assert_eq!(app.selected_project_id(), Some(42));
    }

    #[test]
    fn given_missing_issue_when_reselected_then_tree_focus_is_preserved() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                archived_issues: vec![test_issue(7, IssueStatus::Done, "archived")],
                ..ProjectChildren::default()
            },
        );
        app.tree_sel = 2;

        assert!(!app.select_issue_in_tree(99));

        assert_eq!(app.tree_sel, 2);
    }

    #[test]
    fn given_log_scroll_past_end_when_clamped_then_last_line_is_max() {
        let mut app = test_app();
        app.log_tail = "one\ntwo\nthree".into();
        app.issue_log_scroll = 99;

        app.clamp_issue_log_scroll();

        assert_eq!(app.issue_log_scroll, 2);
    }

    #[test]
    fn given_issue_log_scrolled_when_scroll_down_then_moves_newer() {
        let mut app = test_app();
        app.log_tail = "one\ntwo\nthree\n".to_string();
        app.issue_log_scroll = 2;

        app.scroll_issue_log(-1);

        assert_eq!(app.issue_log_scroll, 1);
    }

    #[test]
    fn given_issue_log_when_jump_top_and_bottom_then_offsets_match() {
        let mut app = test_app();
        app.log_tail = "one\ntwo\nthree\n".to_string();

        app.jump_issue_log_top();
        assert_eq!(app.issue_log_scroll, 2);

        app.jump_issue_log_bottom();
        assert_eq!(app.issue_log_scroll, 0);
    }

    #[tokio::test]
    async fn given_selected_issue_log_event_when_handled_then_log_tail_refreshes() {
        let socket = issue_log_socket_path("selected-issue-log");
        let server_socket = socket.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            serve_selected_issue_log(server_socket, ready_tx).await;
        });
        ready_rx.await.unwrap();
        let mut app = App::new(socket);
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                issues: vec![test_issue(7, IssueStatus::Working, "active")],
                ..ProjectChildren::default()
            },
        );
        app.select_issue_in_tree(7);
        app.log_tail = "stale".into();
        app.log_tail_path = Some("target/agent.log".into());

        app.on_event(Event::IssueLog {
            issue_id: 7,
            phase: "fix".into(),
            chunk: "new".into(),
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(app.log_tail, "fresh\nlog");
        assert_eq!(app.detail.runs.len(), 1);
        assert_eq!(app.issue_log_scroll, 0);
    }

    #[tokio::test]
    async fn given_other_issue_log_event_when_handled_then_no_log_refresh_is_requested() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.children.insert(
            42,
            ProjectChildren {
                issues: vec![test_issue(7, IssueStatus::Working, "active")],
                ..ProjectChildren::default()
            },
        );
        app.select_issue_in_tree(7);
        app.log_tail = "current".into();

        app.on_event(Event::IssueLog {
            issue_id: 8,
            phase: "fix".into(),
            chunk: "new".into(),
        })
        .await
        .unwrap();

        assert_eq!(app.log_tail, "current");
    }

    // --- archive/log fixtures --------------------------------------------

    fn test_project() -> Project {
        let mut project = project_fixture();
        project.id = 42;
        project.name = "demo".into();
        project.schedule_cron = Some("*/15 * * * *".into());
        project.skill_path = Some("skills".into());
        project
    }

    fn test_issue(id: i64, status: IssueStatus, title: &str) -> Issue {
        let mut issue = issue_fixture();
        issue.id = id;
        issue.project_id = 42;
        issue.status = status;
        issue.title = title.into();
        issue
    }

    fn backlog_fixture() -> BacklogItem {
        BacklogItem {
            id: 1,
            project_id: 1,
            text: "queued work".into(),
            source: Source::Human,
            approval: auwsx_core::backlog::Approval::Approved,
            origin_routine_id: None,
            consumed_issue_id: None,
            created_at: 1,
            resolved_at: None,
        }
    }

    #[tokio::test]
    async fn given_issue_detail_entered_from_left_when_returning_then_focus_goes_left(
    ) -> anyhow::Result<()> {
        let mut app = test_app();

        app.enter_issue_detail(Focus::Left, None);
        app.apply(Action::Back).await?;

        assert_eq!(app.focus, Focus::Left);
        Ok(())
    }

    #[tokio::test]
    async fn given_issue_detail_entered_from_kanban_when_returning_then_focus_goes_kanban(
    ) -> anyhow::Result<()> {
        let mut app = test_app();

        app.tree_sel = 7;
        app.enter_issue_detail(Focus::ProjectKanban, Some(3));
        app.apply(Action::Back).await?;

        assert_eq!(app.focus, Focus::ProjectKanban);
        assert_eq!(app.tree_sel, 3);
        Ok(())
    }

    #[test]
    fn given_kanban_issue_with_collapsed_left_tree_when_opened_then_detail_uses_card_issue() {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.focus = Focus::ProjectKanban;
        app.tree_sel = 0;

        assert!(app.enter_issue_detail_for_issue(7, Focus::ProjectKanban, Some(app.tree_sel)));

        assert_eq!(app.focus, Focus::IssueDetail);
        assert_eq!(app.selected_issue_id(), Some(7));
        assert_eq!(app.issue_return_focus, Focus::ProjectKanban);
        assert_eq!(app.issue_return_tree_sel, Some(0));
        assert_eq!(app.selected_tree_item(), Some(TreeItem::Project(1)));
    }

    #[test]
    fn given_open_issue_run_when_app_projects_kanban_then_card_activity_is_agent() {
        let mut app = test_app();
        app.projects.push(project_fixture());
        app.children.insert(
            1,
            ProjectChildren {
                issues: vec![issue_fixture()],
                ..ProjectChildren::default()
            },
        );
        app.recent_agent_runs = vec![test_agent_run(99, 7, Some("target/agent.log"))];

        let card = app
            .kanban_cards()
            .into_iter()
            .find(|card| card.item() == ui::vm::KanbanItem::Issue(7))
            .expect("issue card");

        match card {
            ui::vm::KanbanCard::Issue { activity, .. } => assert_eq!(activity, Some("agent")),
            other => panic!("expected issue card, got {other:?}"),
        }
    }

    // --- App::repo_suggestions -------------------------------------------

    fn test_app() -> App {
        App::new(std::path::PathBuf::from(
            "target/nonexistent-auwsx-test.sock",
        ))
    }

    #[test]
    fn given_expanded_project_when_tree_label_then_counts_hidden() {
        let label = project_tree_label("p", "manual", &ProjectChildren::default(), true);

        assert_eq!(label, "p  manual");
    }

    #[test]
    fn given_collapsed_project_when_tree_label_then_letter_counts_shown() {
        let label = project_tree_label("p", "manual", &ProjectChildren::default(), false);

        assert_eq!(label, "p  manual  R0 B0 I0");
    }

    #[test]
    fn given_project_row_when_capabilities_requested_then_remote_hint_visible() {
        let mut app = test_app();
        app.projects = vec![test_project()];
        app.expanded.insert(42);
        app.children.insert(42, ProjectChildren::default());
        app.tree_sel = app
            .tree_rows()
            .iter()
            .position(|row| matches!(row.item, TreeItem::Project(42)))
            .unwrap();

        assert!(app.capabilities().has(CapabilityAction::Remote));
        assert!(app
            .capabilities()
            .hints
            .iter()
            .any(|hint| hint.label == "(R)emote"));
    }

    #[test]
    fn given_no_remote_config_when_form_built_then_defaults_are_safe_off() {
        let form = Form::project_remote_config(42, None);

        assert_eq!(form.get("remote_provider"), "github");
        assert_eq!(form.get("remote_api_base_url"), "https://api.github.com");
        assert_eq!(form.get("remote_auth_kind"), "token_env");
        assert_eq!(form.get("remote_inbound_auwsx_run"), "false");
        assert_eq!(form.get("remote_pr_merge"), "false");
    }

    #[test]
    fn given_existing_remote_config_when_form_built_then_values_roundtrip() {
        let config = remote_config_fixture();
        let form = Form::project_remote_config(42, Some(&config));

        assert_eq!(form.get("remote_url"), "https://github.com/acme/repo");
        assert_eq!(form.get("remote_owner"), "acme");
        assert_eq!(form.get("remote_repo"), "repo");
        assert_eq!(form.get("remote_auth_kind"), "none");
        assert_eq!(form.get("remote_inbound_auwsx_run"), "true");
        assert_eq!(form.get("remote_pr_merge"), "true");
        assert_eq!(form.get("remote_required_checks"), "require_green");
    }

    fn repo_field(value: &str) -> FormField {
        field("repo_path", value, false)
    }

    fn arsenal_preset(name: &str, review: Option<&str>) -> ArsenalPreset {
        ArsenalPreset {
            id: 1,
            name: name.to_string(),
            main_agent_cmd: format!("{name}-main {{prompt}}"),
            route_agent_cmd: format!("{name}-main {{prompt}}"),
            plan_agent_cmd: format!("{name}-plan {{prompt}}"),
            work_agent_cmd: format!("{name}-work {{prompt}}"),
            review_agent_cmd: review.map(str::to_string),
            builtin: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn project_fixture() -> Project {
        Project {
            id: 1,
            profile_id: 1,
            profile_order: 1,
            name: "p".to_string(),
            repo_path: "/repo".to_string(),
            default_branch: "main".to_string(),
            arsenal_preset_name: None,
            main_agent_cmd: "manual-main".to_string(),
            route_agent_cmd: "manual-main".to_string(),
            plan_agent_cmd: "manual-plan".to_string(),
            work_agent_cmd: "manual-work".to_string(),
            review_agent_cmd: Some("manual-review".to_string()),
            main_agent_cmd_override: Some("manual-main".to_string()),
            route_agent_cmd_override: Some("manual-main".to_string()),
            plan_agent_cmd_override: Some("manual-plan".to_string()),
            work_agent_cmd_override: Some("manual-work".to_string()),
            review_agent_cmd_override: Some("manual-review".to_string()),
            completion_policy: CompletionPolicy::Manual,
            completion_soft_timeout_min: 60,
            plan_gate_timeout_min: 10,
            iteration_timeout_min: 30,
            main_job_timeout_min: 60,
            review_max_rounds: 5,
            conflict_max_attempts: 3,
            max_concurrency: 1,
            schedule_cron: None,
            merge_mode: MergeMode::Local,
            skill_path: None,
            deepsleep_cron: Some("0 0 */30 * *".to_string()),
            last_deepsleep_at: None,
            created_at: 1,
        }
    }

    fn remote_config_fixture() -> ProjectRemoteConfig {
        ProjectRemoteConfig {
            project_id: 42,
            provider: RemoteProvider::Github,
            remote_url: "https://github.com/acme/repo".to_string(),
            owner: "acme".to_string(),
            repo: "repo".to_string(),
            api_base_url: "https://api.github.com".to_string(),
            auth_kind: RemoteAuthKind::None,
            auth_ref: None,
            webhook_secret_ref: None,
            inbound_auwsx_run_enabled: true,
            outbound_issue_create_enabled: true,
            remote_pr_merge_enabled: true,
            agent_comment_sync_enabled: true,
            subtask_comment_sync_enabled: false,
            finding_comment_sync_enabled: true,
            draft_pr_enabled: false,
            required_checks_policy: RequiredChecksPolicy::RequireGreen,
            default_labels: Some("auwsx".to_string()),
            default_assignees: None,
            pr_base_branch: Some("main".to_string()),
            created_at: 1,
            updated_at: 2,
        }
    }

    fn issue_fixture() -> Issue {
        Issue {
            id: 7,
            project_id: 1,
            title: "issue".to_string(),
            description: None,
            agent_summary: None,
            progress_report: None,
            result_report: None,
            status: auwsx_core::state::IssueStatus::Planning,
            branch: None,
            worktree_path: None,
            review_round: 0,
            conflict_attempts: 0,
            wait_until: None,
            absorbed_into_id: None,
            has_pending_steering: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn focus(form: &mut Form, label: &str) {
        form.current = form
            .fields
            .iter()
            .position(|field| field.label == label)
            .expect("field exists");
        form.cursor = form.current_len();
    }

    #[test]
    fn given_no_form_when_repo_suggestions_then_empty() {
        let app = test_app();
        assert!(app.repo_suggestions().is_empty());
    }

    #[test]
    fn given_non_project_form_when_repo_suggestions_then_empty() {
        let mut app = test_app();
        app.scanned_repos = vec!["~/foo".to_string()];
        app.form = Some(Form {
            kind: FormKind::Backlog,
            title: "t",
            fields: vec![repo_field("foo")],
            current: 0,
            cursor: 3,
            completion_sel: 0,
            mode: FormMode::Navigate,
        });
        assert!(app.repo_suggestions().is_empty());
    }

    #[test]
    fn given_project_form_on_non_repo_field_when_repo_suggestions_then_empty() {
        let mut app = test_app();
        app.scanned_repos = vec!["~/foo".to_string()];
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![field("name", "foo", false)],
            current: 0,
            cursor: 3,
            completion_sel: 0,
            mode: FormMode::Navigate,
        });
        assert!(app.repo_suggestions().is_empty());
    }

    #[test]
    fn given_project_form_on_repo_field_with_match_when_repo_suggestions_then_non_empty() {
        let mut app = test_app();
        app.scanned_repos = vec!["~/foo".to_string(), "~/bar".to_string()];
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![repo_field("foo")],
            current: 0,
            cursor: 3,
            completion_sel: 0,
            mode: FormMode::Navigate,
        });
        assert!(!app.repo_suggestions().is_empty());
    }

    #[test]
    fn given_project_form_on_repo_field_no_match_when_repo_suggestions_then_empty() {
        let mut app = test_app();
        app.scanned_repos = vec!["~/foo".to_string(), "~/bar".to_string()];
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![repo_field("zzzz")],
            current: 0,
            cursor: 4,
            completion_sel: 0,
            mode: FormMode::Navigate,
        });
        assert!(app.repo_suggestions().is_empty());
    }

    #[test]
    fn given_repo_field_not_at_index_zero_when_repo_suggestions_then_resolves_focused_field() {
        // The focused field is resolved via `form.current`, not hardcoded to 0.
        let mut app = test_app();
        app.scanned_repos = vec!["~/foo".to_string()];
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![field("name", "x", false), repo_field("foo")],
            current: 1,
            cursor: 3,
            completion_sel: 0,
            mode: FormMode::Navigate,
        });
        assert!(!app.repo_suggestions().is_empty());
    }

    #[test]
    fn given_more_than_eight_matches_when_repo_suggestions_then_capped_at_eight() {
        let mut app = test_app();
        app.scanned_repos = (0..20).map(|i| format!("~/foo{i}")).collect();
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![repo_field("foo")],
            current: 0,
            cursor: 3,
            completion_sel: 0,
            mode: FormMode::Navigate,
        });
        assert_eq!(app.repo_suggestions().len(), 8);
    }

    // --- App::arsenal_suggestions -----------------------------------------

    #[test]
    fn given_project_form_default_when_created_then_arsenal_field_is_blank() {
        let form = Form::project();
        let arsenal = form
            .fields
            .iter()
            .find(|field| field.label == "arsenal")
            .expect("arsenal field exists");
        assert_eq!(arsenal.value, "");
    }

    #[test]
    fn given_project_form_when_created_then_command_fields_are_not_shown() {
        let form = Form::project();

        assert!(form
            .fields
            .iter()
            .all(|field| !field.label.ends_with("_cmd")));
        assert_eq!(
            form.fields
                .iter()
                .filter(|field| field.section == "Agents")
                .map(|field| field.label)
                .collect::<Vec<_>>(),
            vec!["arsenal"]
        );
    }

    #[tokio::test]
    async fn given_form_cursor_moved_left_when_char_typed_then_inserts_at_cursor(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        app.form = Some(Form::backlog_edit(&BacklogItem {
            id: 1,
            project_id: 1,
            text: "abcd".to_string(),
            source: Source::Human,
            approval: auwsx_core::backlog::Approval::Approved,
            origin_routine_id: None,
            consumed_issue_id: None,
            created_at: 1,
            resolved_at: None,
        }));

        app.handle_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await?;
        app.handle_form_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
            .await?;
        app.handle_form_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT))
            .await?;

        let form = app.form.expect("form remains open");
        assert_eq!(form.get("text"), "abcXd");
        assert_eq!(form.cursor, 4);
        assert_eq!(form.mode, FormMode::Edit);
        Ok(())
    }

    #[tokio::test]
    async fn given_form_cursor_when_backspace_and_delete_then_remove_around_cursor(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        let mut form = Form::backlog();
        form.set_current_value("abcd".to_string());
        form.cursor = 2;
        app.form = Some(form);

        app.handle_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await?;
        app.handle_form_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .await?;
        app.handle_form_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE))
            .await?;

        let form = app.form.expect("form remains open");
        assert_eq!(form.get("text"), "ad");
        assert_eq!(form.cursor, 1);
        Ok(())
    }

    #[tokio::test]
    async fn given_form_navigate_mode_when_char_typed_then_value_is_unchanged() -> anyhow::Result<()>
    {
        let mut app = test_app();
        app.form = Some(Form::backlog_edit(&BacklogItem {
            id: 1,
            project_id: 1,
            text: "abcd".to_string(),
            source: Source::Human,
            approval: auwsx_core::backlog::Approval::Approved,
            origin_routine_id: None,
            consumed_issue_id: None,
            created_at: 1,
            resolved_at: None,
        }));

        app.handle_form_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT))
            .await?;

        let form = app.form.expect("form remains open");
        assert_eq!(form.get("text"), "abcd");
        assert_eq!(form.mode, FormMode::Navigate);
        Ok(())
    }

    #[tokio::test]
    async fn given_form_edit_mode_when_enter_pressed_then_returns_to_navigation(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        app.form = Some(Form::backlog());

        app.handle_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await?;
        app.handle_form_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await?;
        app.handle_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await?;

        let form = app.form.expect("form remains open");
        assert_eq!(form.get("text"), "x");
        assert_eq!(form.mode, FormMode::Navigate);
        Ok(())
    }

    #[tokio::test]
    async fn given_non_free_combo_when_char_typed_then_value_is_unchanged() -> anyhow::Result<()> {
        let mut app = test_app();
        let mut form = Form::global_settings(None);
        focus(&mut form, "memory_preset");
        let before = form.get("memory_preset").to_string();
        app.form = Some(form);

        app.handle_form_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await?;

        let form = app.form.expect("form remains open");
        assert_eq!(form.get("memory_preset"), before);
        Ok(())
    }

    #[test]
    fn given_status_deadline_in_past_when_clear_expired_status_then_status_is_empty() {
        let mut app = test_app();
        app.status = "cancelled".to_string();
        app.status_until = Some(Instant::now() - Duration::from_millis(1));

        assert!(app.clear_expired_status());

        assert_eq!(app.status, "");
    }

    #[tokio::test]
    async fn given_idle_ui_tick_without_visible_animation_then_no_redraw() -> anyhow::Result<()> {
        let mut app = test_app();
        app.last_poll_at = Instant::now();

        assert!(!app.ui_tick().await?);
        Ok(())
    }

    #[tokio::test]
    async fn given_visible_schedule_countdown_when_ui_tick_then_redraws_without_poll(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        let mut project = project_fixture();
        project.schedule_cron = Some("@tick".to_string());
        app.projects.push(project);
        app.last_poll_at = Instant::now();

        assert!(app.ui_tick().await?);
        Ok(())
    }

    #[tokio::test]
    async fn given_noop_movement_when_applied_then_render_revision_is_unchanged(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        app.focus = Focus::ProjectKanban;
        app.kanban_lane_sel = 0;
        app.kanban_card_sel = 0;
        let before = app.render_revision();

        app.apply(Action::Down).await?;

        assert_eq!(app.render_revision(), before);
        Ok(())
    }

    #[tokio::test]
    async fn given_form_input_when_handled_then_render_revision_changes() -> anyhow::Result<()> {
        let mut app = test_app();
        app.form = Some(Form::backlog());
        let before = app.render_revision();

        app.handle_form_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await?;
        app.handle_form_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await?;

        assert_ne!(app.render_revision(), before);
        Ok(())
    }

    #[tokio::test]
    async fn given_settings_view_when_moving_down_then_selected_row_changes() -> anyhow::Result<()>
    {
        let mut app = test_app();
        app.view = View::Config;
        app.arsenal_presets = vec![arsenal_preset("codex", None)];

        app.apply(Action::Down).await?;

        assert_eq!(app.selected_settings_row(), SettingsRow::ArsenalOverview);
        Ok(())
    }

    #[tokio::test]
    async fn given_settings_arsenal_row_when_edit_then_opens_arsenal_form() -> anyhow::Result<()> {
        let mut app = test_app();
        app.view = View::Config;
        app.arsenal_presets = vec![arsenal_preset("codex", None)];
        app.settings_sel = app
            .settings_rows()
            .iter()
            .position(|row| matches!(row, SettingsRow::ArsenalPreset(_)))
            .expect("arsenal preset row");

        app.apply(Action::EditSelected).await?;

        let form = app.form.expect("arsenal form");
        assert_eq!(form.kind, FormKind::ArsenalPreset);
        assert_eq!(form.get("name"), "codex");
        Ok(())
    }

    #[tokio::test]
    async fn given_settings_memory_row_when_edit_then_opens_global_settings_form(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        app.view = View::Config;
        app.memory_presets = vec![MemoryPreset {
            id: 1,
            name: "portable-markdown".to_string(),
            retrieve_kind: "portable".to_string(),
            retrieve_cmd: None,
            save_kind: "portable".to_string(),
            save_cmd: None,
            dream_kind: "portable".to_string(),
            dream_cmd: None,
            deepsleep_kind: "portable".to_string(),
            deepsleep_cmd: None,
            builtin: true,
            created_at: 0,
            updated_at: 0,
        }];
        app.settings_sel = app
            .settings_rows()
            .iter()
            .position(|row| matches!(row, SettingsRow::MemoryPreset(_)))
            .expect("memory preset row");

        app.apply(Action::EditSelected).await?;

        let form = app.form.expect("global settings form");
        assert_eq!(form.kind, FormKind::GlobalSettings);
        assert_eq!(form.get("memory_preset"), "portable-markdown");
        Ok(())
    }

    #[test]
    fn given_project_form_with_arsenal_preset_when_building_add_command_then_sends_only_preset_ref()
    {
        let mut form = Form::project();
        form.set("name", "manual");
        form.set("repo_path", "/repo");
        form.set("arsenal", "codex");
        let mut status = String::new();
        let preset = arsenal_preset("codex", Some("codex-review {prompt}"));

        let cmd = add_project_command_from_form(&form, Some(&preset), &mut status)
            .expect("valid project command");

        match cmd {
            Command::AddProject {
                arsenal_preset_name,
                main_agent_cmd,
                route_agent_cmd,
                plan_agent_cmd,
                work_agent_cmd,
                review_agent_cmd,
                ..
            } => assert_eq!(
                (
                    arsenal_preset_name.as_deref(),
                    main_agent_cmd.as_str(),
                    route_agent_cmd.as_str(),
                    plan_agent_cmd.as_str(),
                    work_agent_cmd.as_str(),
                    review_agent_cmd.as_deref()
                ),
                (Some("codex"), "", "", "", "", None)
            ),
            other => panic!("expected AddProject, got {other:?}"),
        }
    }

    #[test]
    fn given_project_form_without_arsenal_preset_when_building_add_command_then_rejected() {
        let mut form = Form::project();
        form.set("name", "preset");
        form.set("repo_path", "/repo");
        let mut status = String::new();

        let cmd = add_project_command_from_form(&form, None, &mut status);

        assert!(cmd.is_none());
        assert_eq!(status, "select an Arsenal preset first");
    }

    #[test]
    fn given_project_arsenal_ref_when_open_config_then_arsenal_is_primary_choice() {
        let mut app = test_app();
        let preset = arsenal_preset("codex", Some("codex-review {prompt}"));
        let mut project = project_fixture();
        project.arsenal_preset_name = Some("codex".to_string());
        project.main_agent_cmd = preset.main_agent_cmd.clone();
        project.plan_agent_cmd = preset.plan_agent_cmd.clone();
        project.work_agent_cmd = preset.work_agent_cmd.clone();
        project.review_agent_cmd = preset.review_agent_cmd.clone();
        project.main_agent_cmd_override = None;
        project.plan_agent_cmd_override = None;
        project.work_agent_cmd_override = None;
        project.review_agent_cmd_override = None;
        app.arsenal_presets = vec![preset];

        let form = app.project_config_form(&project);

        assert_eq!(form.get("arsenal"), "codex");
        assert!(form
            .fields
            .iter()
            .all(|field| !field.label.ends_with("_cmd")));
    }

    #[test]
    fn given_project_arsenal_ref_with_override_when_open_config_then_override_is_hidden() {
        let mut app = test_app();
        let preset = arsenal_preset("codex", Some("codex-review {prompt}"));
        let mut project = project_fixture();
        project.arsenal_preset_name = Some("codex".to_string());
        project.main_agent_cmd = "manual-main {prompt}".to_string();
        project.plan_agent_cmd = preset.plan_agent_cmd.clone();
        project.work_agent_cmd = preset.work_agent_cmd.clone();
        project.review_agent_cmd = preset.review_agent_cmd.clone();
        project.main_agent_cmd_override = Some("manual-main {prompt}".to_string());
        project.plan_agent_cmd_override = None;
        project.work_agent_cmd_override = None;
        project.review_agent_cmd_override = None;
        app.arsenal_presets = vec![preset];

        let form = app.project_config_form(&project);

        assert_eq!(form.get("arsenal"), "codex");
        assert!(form
            .fields
            .iter()
            .all(|field| !field.label.ends_with("_cmd")));
    }

    #[test]
    fn given_no_form_when_arsenal_suggestions_then_empty() {
        let app = test_app();
        assert!(app.arsenal_suggestions().is_empty());
    }

    #[test]
    fn given_non_project_form_when_arsenal_suggestions_then_empty() {
        let mut app = test_app();
        app.arsenal_presets = vec![arsenal_preset("codex", None)];
        app.form = Some(Form {
            kind: FormKind::Backlog,
            title: "t",
            fields: vec![field("arsenal", "co", true)],
            current: 0,
            cursor: 2,
            completion_sel: 0,
            mode: FormMode::Navigate,
        });
        assert!(app.arsenal_suggestions().is_empty());
    }

    #[test]
    fn given_project_form_on_non_arsenal_field_when_arsenal_suggestions_then_empty() {
        let mut app = test_app();
        app.arsenal_presets = vec![arsenal_preset("codex", None)];
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![field("name", "co", false)],
            current: 0,
            cursor: 2,
            completion_sel: 0,
            mode: FormMode::Navigate,
        });
        assert!(app.arsenal_suggestions().is_empty());
    }

    #[test]
    fn given_project_form_arsenal_query_when_arsenal_suggestions_then_case_insensitive_match() {
        let mut app = test_app();
        app.arsenal_presets = vec![
            arsenal_preset("Codex", None),
            arsenal_preset("claude", None),
        ];
        let mut form = Form::project();
        focus(&mut form, "arsenal");
        form.set("arsenal", "cod");
        app.form = Some(form);
        assert_eq!(app.arsenal_suggestions(), vec!["Codex".to_string()]);
    }

    #[test]
    fn given_project_config_form_with_many_matches_when_arsenal_suggestions_then_capped_at_eight() {
        let mut app = test_app();
        app.arsenal_presets = (0..20)
            .map(|i| arsenal_preset(&format!("codex-{i}"), None))
            .collect();
        let mut form = Form::project_config(&project_fixture());
        focus(&mut form, "arsenal");
        form.set("arsenal", "codex");
        app.form = Some(form);
        assert_eq!(app.arsenal_suggestions().len(), 8);
    }

    #[tokio::test]
    async fn given_arsenal_field_when_tab_accepts_preset_then_commands_are_blank_overrides(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        app.arsenal_presets = vec![arsenal_preset("codex", Some("codex-review {prompt}"))];
        let mut form = Form::project();
        focus(&mut form, "arsenal");
        form.set("arsenal", "cod");
        app.form = Some(form);

        app.handle_form_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await?;

        let form = app.form.expect("form remains open");
        assert_eq!(form.get("arsenal"), "codex");
        assert!(form
            .fields
            .iter()
            .all(|field| !field.label.ends_with("_cmd")));
        Ok(())
    }

    #[tokio::test]
    async fn given_accepted_preset_without_review_when_tab_accepts_then_no_command_fields_are_added(
    ) -> anyhow::Result<()> {
        let mut app = test_app();
        app.arsenal_presets = vec![arsenal_preset("codex", None)];
        let mut form = Form::project();
        focus(&mut form, "arsenal");
        form.set("arsenal", "cod");
        app.form = Some(form);

        app.handle_form_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await?;

        let form = app.form.expect("form remains open");
        assert!(form
            .fields
            .iter()
            .all(|field| !field.label.ends_with("_cmd")));
        Ok(())
    }
}

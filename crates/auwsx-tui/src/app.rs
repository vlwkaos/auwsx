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
use auwsx_core::db::findings::Finding;
use auwsx_core::db::issues::Issue;
use auwsx_core::db::projects::{CompletionPolicy, MergeMode, Project};
use auwsx_core::db::scheduler_runs::SchedulerRun;
use auwsx_core::db::subtasks::Subtask;
use auwsx_core::events::Event;
use auwsx_core::ipc::{self, Command, Response};
use auwsx_core::main_jobs::MainJob;
use auwsx_core::routines::{Routine, RoutineType};
use auwsx_core::steering::Steering;
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The top-level views, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Overview,
    Issue,
    Backlog,
    Logs,
    Config,
}

impl View {
    pub const ORDER: [View; 5] = [
        View::Overview,
        View::Issue,
        View::Backlog,
        View::Logs,
        View::Config,
    ];

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
}

impl TreeItem {
    /// The project this node belongs to.
    pub fn project_id(&self) -> i64 {
        match self {
            TreeItem::Project(id)
            | TreeItem::RoutinesRoot(id)
            | TreeItem::BacklogRoot(id)
            | TreeItem::IssuesRoot(id)
            | TreeItem::Routine { project_id: id, .. }
            | TreeItem::Backlog { project_id: id, .. }
            | TreeItem::Issue { project_id: id, .. } => *id,
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
    pub issues: Vec<Issue>,
}

/// Everything the issue-detail pane shows, fetched together.
#[derive(Default)]
pub struct IssueDetail {
    pub issue: Option<Issue>,
    pub subtasks: Vec<Subtask>,
    pub findings: Vec<Finding>,
    pub steering: Vec<Steering>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormKind {
    Project,
    ProjectConfig,
    Backlog,
    BacklogEdit(i64),
    Routine,
    RoutineEdit(i64),
    Issue,
    Subtask,
    Steering,
}

#[derive(Debug, Clone)]
pub struct Form {
    pub kind: FormKind,
    pub title: &'static str,
    pub fields: Vec<FormField>,
    pub current: usize,
}

#[derive(Debug, Clone)]
pub struct FormField {
    pub key: &'static str,
    pub label: &'static str,
    pub value: String,
    pub cursor: usize,
    pub optional: bool,
}

impl FormField {
    fn new(key: &'static str, label: &'static str, value: &str, optional: bool) -> Self {
        Self {
            key,
            label,
            value: value.to_string(),
            cursor: value.chars().count(),
            optional,
        }
    }

    fn char_len(&self) -> usize {
        self.value.chars().count()
    }

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.char_len());
    }

    pub(crate) fn cursor_byte_index(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor.min(self.char_len()))
            .map(|(idx, _)| idx)
            .unwrap_or(self.value.len())
    }

    fn byte_index_at(&self, char_pos: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_pos.min(self.char_len()))
            .map(|(idx, _)| idx)
            .unwrap_or(self.value.len())
    }

    fn set_value(&mut self, value: String) {
        self.value = value;
        self.cursor = self.char_len();
    }

    fn move_left(&mut self) {
        self.clamp_cursor();
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.clamp_cursor();
        self.cursor = (self.cursor + 1).min(self.char_len());
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.char_len();
    }

    fn insert_char(&mut self, c: char) {
        self.clamp_cursor();
        let byte_idx = self.cursor_byte_index();
        self.value.insert(byte_idx, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let start = self.byte_index_at(self.cursor - 1);
        let end = self.byte_index_at(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.char_len() {
            return;
        }
        let start = self.byte_index_at(self.cursor);
        let end = self.byte_index_at(self.cursor + 1);
        self.value.replace_range(start..end, "");
    }
}

impl Form {
    fn project() -> Self {
        let repo = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let codex = codex::DEFAULT_CMD.to_string();
        Self {
            kind: FormKind::Project,
            title: "New project",
            fields: vec![
                project_field("name", "Name", "", false),
                project_field("repo_path", "Repository", &repo, false),
                project_field("branch", "Default branch", "main", false),
                project_field("main_cmd", "Main command", &codex, false),
                project_field("plan_cmd", "Plan command", &codex, false),
                project_field("work_cmd", "Work command", &codex, false),
                project_field("review_cmd", "Review command", &codex, true),
            ],
            current: 0,
        }
    }

    fn project_config(project: &Project) -> Self {
        Self {
            kind: FormKind::ProjectConfig,
            title: "Project config",
            fields: vec![
                project_field("name", "Name", &project.name, false),
                project_field("repo_path", "Repository", &project.repo_path, false),
                project_field("branch", "Default branch", &project.default_branch, false),
                project_field("main_cmd", "Main command", &project.main_agent_cmd, false),
                project_field("plan_cmd", "Plan command", &project.plan_agent_cmd, false),
                project_field("work_cmd", "Work command", &project.work_agent_cmd, false),
                project_field(
                    "review_cmd",
                    "Review command",
                    project.review_agent_cmd.as_deref().unwrap_or(""),
                    true,
                ),
                project_field(
                    "completion",
                    "Completion policy",
                    project.completion_policy.as_str(),
                    false,
                ),
                project_field(
                    "plan_gate",
                    "Plan gate timeout",
                    &project.plan_gate_timeout_min.to_string(),
                    false,
                ),
                project_field(
                    "complete_gate",
                    "Completion timeout",
                    &project.completion_soft_timeout_min.to_string(),
                    false,
                ),
                project_field(
                    "iter_timeout",
                    "Iteration timeout",
                    &project.iteration_timeout_min.to_string(),
                    false,
                ),
                project_field(
                    "main_job_timeout",
                    "Main job timeout",
                    &project.main_job_timeout_min.to_string(),
                    false,
                ),
                project_field(
                    "review_rounds",
                    "Review rounds",
                    &project.review_max_rounds.to_string(),
                    false,
                ),
                project_field(
                    "conflict_attempts",
                    "Conflict attempts",
                    &project.conflict_max_attempts.to_string(),
                    false,
                ),
                project_field(
                    "concurrency",
                    "Concurrency",
                    &project.max_concurrency.to_string(),
                    false,
                ),
                project_field(
                    "schedule_min",
                    "Schedule interval",
                    &project
                        .schedule_interval_min
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    true,
                ),
                project_field(
                    "merge_mode",
                    "Merge mode",
                    project.merge_mode.as_str(),
                    false,
                ),
                project_field(
                    "skill_path",
                    "Skills path",
                    project.skill_path.as_deref().unwrap_or(""),
                    true,
                ),
                project_field(
                    "deepsleep_days",
                    "Deepsleep interval",
                    &project.deepsleep_interval_days.to_string(),
                    false,
                ),
            ],
            current: 0,
        }
    }

    fn backlog() -> Self {
        Self {
            kind: FormKind::Backlog,
            title: "New backlog item",
            fields: vec![field("text", "", false)],
            current: 0,
        }
    }

    fn backlog_edit(item: &BacklogItem) -> Self {
        Self {
            kind: FormKind::BacklogEdit(item.id),
            title: "Edit backlog item",
            fields: vec![field("text", &item.text, false)],
            current: 0,
        }
    }

    fn routine() -> Self {
        Self {
            kind: FormKind::Routine,
            title: "New routine",
            fields: vec![
                field("name", "", false),
                field("type", "report", false),
                field("cron", "0 9 * * *", false),
                field("prompt", "", false),
                field("writable_paths", "", true),
                field("enabled", "true", false),
            ],
            current: 0,
        }
    }

    fn routine_edit(routine: &Routine) -> Self {
        Self {
            kind: FormKind::RoutineEdit(routine.id),
            title: "Edit routine",
            fields: vec![
                field("name", &routine.name, false),
                field("type", routine.routine_type.as_str(), false),
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
        }
    }

    fn issue() -> Self {
        Self {
            kind: FormKind::Issue,
            title: "New issue",
            fields: vec![field("title", "", false), field("description", "", true)],
            current: 0,
        }
    }

    fn subtask(next_ord: i64) -> Self {
        Self {
            kind: FormKind::Subtask,
            title: "New subtask",
            fields: vec![
                field("ord", &next_ord.to_string(), false),
                field("text", "", false),
            ],
            current: 1,
        }
    }

    fn steering() -> Self {
        Self {
            kind: FormKind::Steering,
            title: "New steering note",
            fields: vec![field("note", "", false)],
            current: 0,
        }
    }

    fn get(&self, label: &str) -> String {
        self.fields
            .iter()
            .find(|f| f.key == label)
            .map(|f| f.value.trim().to_string())
            .unwrap_or_default()
    }

    fn opt(&self, label: &str) -> Option<String> {
        let s = self.get(label);
        (!s.is_empty()).then_some(s)
    }

    fn missing_required(&self) -> Option<&'static str> {
        self.fields
            .iter()
            .find(|f| !f.optional && f.value.trim().is_empty())
            .map(|f| f.label)
    }

    fn label_for<'a>(&'a self, key: &'a str) -> &'a str {
        self.fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.label)
            .unwrap_or(key)
    }
}

fn field(label: &'static str, value: &str, optional: bool) -> FormField {
    FormField::new(label, label, value, optional)
}

fn project_field(key: &'static str, label: &'static str, value: &str, optional: bool) -> FormField {
    FormField::new(key, label, value, optional)
}

fn parse_i64(form: &Form, label: &'static str, status: &mut String) -> Option<i64> {
    match form.get(label).parse::<i64>() {
        Ok(value) => Some(value),
        Err(_) => {
            *status = format!("{} must be an integer", form.label_for(label));
            None
        }
    }
}

fn parse_opt_i64(form: &Form, label: &'static str, status: &mut String) -> Option<Option<i64>> {
    let raw = form.get(label);
    if raw.is_empty() {
        return Some(None);
    }
    match raw.parse::<i64>() {
        Ok(value) => Some(Some(value)),
        Err(_) => {
            *status = format!("{} must be blank or an integer", form.label_for(label));
            None
        }
    }
}

fn parse_bool(form: &Form, label: &'static str, status: &mut String) -> Option<bool> {
    match form.get(label).as_str() {
        "true" | "yes" | "1" | "on" => Some(true),
        "false" | "no" | "0" | "off" => Some(false),
        _ => {
            *status = format!("{} must be true or false", form.label_for(label));
            None
        }
    }
}

fn parse_choice<T>(
    form: &Form,
    label: &'static str,
    expected: &str,
    parse: impl FnOnce(&str) -> Option<T>,
    status: &mut String,
) -> Option<T> {
    match parse(&form.get(label)) {
        Some(value) => Some(value),
        None => {
            *status = format!("{} must be {expected}", form.label_for(label));
            None
        }
    }
}

fn parse_routine_type(form: &Form, status: &mut String) -> Option<RoutineType> {
    parse_choice(
        form,
        "type",
        "report, idea, or knowledge",
        RoutineType::from_str,
        status,
    )
}

pub struct App {
    pub socket: PathBuf,
    pub view: View,

    pub projects: Vec<Project>,
    /// Index of the "active" project — the one the cursor currently sits in.
    /// Kept in sync with `tree_sel` so the detail pane and per-project activity
    /// (recent runs, config) track the cursor across project boundaries.
    pub proj_sel: usize,
    /// Eagerly-loaded children for every project, keyed by project id.
    pub children: HashMap<i64, ProjectChildren>,
    /// Project ids whose children are expanded in the tree.
    pub expanded: HashSet<i64>,
    pub issue_sel: usize,
    pub backlog_sel: usize,
    pub tree_sel: usize,
    pub detail: IssueDetail,
    pub recent_agent_runs: Vec<AgentRun>,
    pub recent_main_jobs: Vec<MainJob>,
    pub recent_scheduler_runs: Vec<SchedulerRun>,
    pub log_tail: String,
    pub log_tail_path: Option<String>,

    /// Most-recent-last ring of formatted daemon events for the Logs view.
    pub log: VecDeque<String>,
    /// Whether the live event subscription is currently attached.
    pub connected: bool,
    /// A transient status/error message shown in the footer.
    pub status: String,
    /// Active inline data-entry form, rendered as a modal overlay.
    pub form: Option<Form>,
    /// Git repos discovered under `$HOME` (display paths), for the New-project
    /// form's `repo_path` completion. Populated once by a background scan.
    pub scanned_repos: Vec<String>,
}

const LOG_CAP: usize = 500;

impl App {
    pub fn new(socket: PathBuf) -> Self {
        App {
            socket,
            view: View::Overview,
            projects: Vec::new(),
            proj_sel: 0,
            children: HashMap::new(),
            expanded: HashSet::new(),
            issue_sel: 0,
            backlog_sel: 0,
            tree_sel: 0,
            detail: IssueDetail::default(),
            recent_agent_runs: Vec::new(),
            recent_main_jobs: Vec::new(),
            recent_scheduler_runs: Vec::new(),
            log_tail: String::new(),
            log_tail_path: None,
            log: VecDeque::new(),
            connected: false,
            status: String::new(),
            form: None,
            scanned_repos: Vec::new(),
        }
    }

    /// Fuzzy-completion suggestions for the project `repo_path` field, based on
    /// the current field text. Empty unless a project form is open on that field.
    /// Capped to keep the dropdown short.
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
        if field.key != "repo_path" {
            return Vec::new();
        }
        crate::repo_scan::filter_repos(&field.value, &self.scanned_repos, 8)
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

    /// The currently-selected issue id (if the cursor is on one).
    pub fn selected_issue_id(&self) -> Option<i64> {
        match self.selected_tree_item() {
            Some(TreeItem::Issue { id, .. }) => Some(id),
            _ => self.issues().get(self.issue_sel).map(|i| i.id),
        }
    }

    fn selected_backlog_id(&self) -> Option<i64> {
        match self.selected_tree_item() {
            Some(TreeItem::Backlog { id, .. }) => Some(id),
            _ => self.backlog().get(self.backlog_sel).map(|b| b.id),
        }
    }

    pub fn tree_rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        if self.projects.is_empty() {
            return rows;
        }
        for p in &self.projects {
            let expanded = self.expanded.contains(&p.id);
            let empty = ProjectChildren::default();
            let kids = self.children.get(&p.id).unwrap_or(&empty);
            rows.push(TreeRow {
                item: TreeItem::Project(p.id),
                label: format!(
                    "{}  (r{} b{} i{})",
                    p.name,
                    kids.routines.len(),
                    kids.backlog.len(),
                    kids.issues.len()
                ),
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
                let consumed = if b.consumed_issue_id.is_some() {
                    "*"
                } else {
                    " "
                };
                rows.push(TreeRow {
                    item: TreeItem::Backlog {
                        project_id: p.id,
                        id: b.id,
                    },
                    label: format!(
                        "{} {:<9} #{:<3} {}",
                        consumed,
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
                    label: format!("#{: <3} {:<13} {}", i.id, i.status.as_str(), i.title),
                    depth: 2,
                });
            }
        }
        rows
    }

    pub fn selected_tree_item(&self) -> Option<TreeItem> {
        self.tree_rows().get(self.tree_sel).map(|r| r.item.clone())
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
        match self.selected_tree_item()? {
            TreeItem::Issue { project_id, id } => self
                .children_of(project_id)?
                .issues
                .iter()
                .find(|i| i.id == id),
            _ => self.issues().get(self.issue_sel),
        }
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
        self.refresh_projects().await?;
        for pid in self.project_ids() {
            self.refresh_project_children(pid).await?;
        }
        self.clamp_tree();
        self.sync_active_project();
        self.refresh_detail().await?;
        self.refresh_activity().await?;
        Ok(())
    }

    fn project_ids(&self) -> Vec<i64> {
        self.projects.iter().map(|p| p.id).collect()
    }

    async fn refresh_projects(&mut self) -> Result<()> {
        if let Response::Projects(ps) = self.req(Command::ListProjects).await? {
            self.projects = ps;
            if self.proj_sel >= self.projects.len() {
                self.proj_sel = self.projects.len().saturating_sub(1);
            }
            // Drop caches for projects that no longer exist; first sight of a
            // project auto-expands it so the tree is not a wall of collapsed rows.
            let live: HashSet<i64> = self.projects.iter().map(|p| p.id).collect();
            self.children.retain(|id, _| live.contains(id));
            self.expanded.retain(|id| live.contains(id));
            if self.expanded.is_empty() {
                self.expanded.extend(live);
            }
        }
        Ok(())
    }

    /// Load routines + backlog + issues for one project into the cache.
    async fn refresh_project_children(&mut self, project_id: i64) -> Result<()> {
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
            // Stable, readable order: oldest first (creation order == id order).
            is.sort_by_key(|i| i.id);
            kids.issues = is;
        }
        self.children.insert(project_id, kids);
        Ok(())
    }

    /// Refresh just the active project's children (used after local mutations).
    async fn refresh_issues(&mut self) -> Result<()> {
        if let Some(pid) = self.selected_project_id() {
            self.refresh_project_children(pid).await?;
        }
        let len = self.issues().len();
        if self.issue_sel >= len {
            self.issue_sel = len.saturating_sub(1);
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
            return Ok(());
        };
        if let Response::AgentRuns(runs) = self
            .req(Command::ListAgentRunsByIssue { issue_id: iid })
            .await?
        {
            if let Some(run) = runs.iter().rev().find(|r| r.log_path.is_some()) {
                match self
                    .req(Command::TailAgentRunLog {
                        agent_run_id: run.id,
                        max_bytes: 8 * 1024,
                    })
                    .await
                {
                    Ok(Response::LogTail { path, text }) => {
                        self.log_tail = text;
                        self.log_tail_path = Some(path);
                    }
                    _ => {
                        self.log_tail.clear();
                        self.log_tail_path = run.log_path.clone();
                    }
                }
            } else {
                self.log_tail.clear();
                self.log_tail_path = None;
            }
        }
        Ok(())
    }

    // --- action handling ----------------------------------------------------

    /// Apply one decoded action. Returns `true` when the app should quit.
    async fn apply(&mut self, action: Action) -> Result<bool> {
        self.status.clear();
        match action {
            Action::Quit => return Ok(true),
            Action::Down => self.move_sel(1).await?,
            Action::Up => self.move_sel(-1).await?,
            Action::Drill => self.drill().await?,
            Action::NextView => self.set_view(self.view.step(1)).await?,
            Action::PrevView => self.set_view(self.view.step(-1)).await?,
            Action::Back => {
                if self.view == View::Issue {
                    self.set_view(View::Overview).await?;
                }
            }
            Action::Refresh => self.refresh_all().await?,
            Action::NewProject => self.form = Some(Form::project()),
            Action::EditConfig => {
                if let Some(project) = self.projects.get(self.proj_sel) {
                    self.form = Some(Form::project_config(project));
                } else {
                    self.status = "select or create a project first".into();
                }
            }
            Action::EditSelected => match self.selected_tree_item() {
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
                _ => self.status = "select a backlog item or routine to edit".into(),
            },
            Action::NewBacklog => {
                if matches!(
                    self.selected_tree_item(),
                    Some(TreeItem::RoutinesRoot(_) | TreeItem::Routine { .. })
                ) {
                    if self.selected_project_id().is_some() {
                        self.form = Some(Form::routine());
                    } else {
                        self.status = "select or create a project first".into();
                    }
                } else if self.selected_project_id().is_some() {
                    self.form = Some(Form::backlog());
                } else {
                    self.status = "select or create a project first".into();
                }
            }
            Action::NewIssue => {
                if self.selected_project_id().is_some() {
                    self.form = Some(Form::issue());
                } else {
                    self.status = "select or create a project first".into();
                }
            }
            Action::NewSubtask => {
                if self.selected_issue_id().is_some() {
                    self.form = Some(Form::subtask(self.detail.subtasks.len() as i64 + 1));
                } else {
                    self.status = "select an issue first".into();
                }
            }
            Action::NewSteering => {
                if self.selected_issue_id().is_some() {
                    self.form = Some(Form::steering());
                } else {
                    self.status = "select an issue first".into();
                }
            }
            Action::Approve => {
                if let Some(id) = self.selected_backlog_id() {
                    self.req_ok(Command::ApproveBacklog { item_id: id }, "approve")
                        .await;
                    self.refresh_backlog().await?;
                }
            }
            Action::Dismiss => {
                if let Some(id) = self.selected_backlog_id() {
                    self.req_ok(Command::DismissBacklog { item_id: id }, "dismiss")
                        .await;
                    self.refresh_backlog().await?;
                }
            }
            Action::Triage => {
                if let Some(pid) = self.selected_project_id() {
                    match self.req(Command::Triage { project_id: pid }).await {
                        Ok(Response::Triaged { created_issue_ids }) => {
                            self.status =
                                format!("triaged: {} new issue(s)", created_issue_ids.len());
                        }
                        Ok(Response::Err { message }) => {
                            self.status = format!("triage failed: {message}")
                        }
                        Ok(_) => {}
                        Err(e) => self.status = format!("triage failed: {e}"),
                    }
                    self.refresh_backlog().await?;
                    self.refresh_issues().await?;
                }
            }
            Action::Execute => self.execute_selected().await?,
            Action::ToggleRoutine => self.toggle_selected_routine().await?,
        }
        Ok(false)
    }

    async fn execute_selected(&mut self) -> Result<()> {
        match self.selected_tree_item() {
            Some(TreeItem::Project(project_id)) => {
                self.req_ok(Command::RunSchedulerOnce { project_id }, "scheduler tick")
                    .await;
            }
            Some(TreeItem::Backlog { id: item_id, .. }) => {
                match self.req(Command::RunBacklogNow { item_id }).await {
                    Ok(Response::RanIssue { issue_id }) => {
                        self.status = format!("running issue #{issue_id}");
                    }
                    Ok(Response::Err { message }) => self.status = format!("run failed: {message}"),
                    Ok(_) => self.status = "run failed: unexpected response".into(),
                    Err(e) => self.status = format!("run failed: {e}"),
                }
            }
            Some(TreeItem::Issue { id: issue_id, .. }) => {
                match self.req(Command::RunIssueNow { issue_id }).await {
                    Ok(Response::RanIssue { issue_id }) => {
                        self.status = format!("running issue #{issue_id}");
                    }
                    Ok(Response::Err { message }) => self.status = format!("run failed: {message}"),
                    Ok(_) => self.status = "run failed: unexpected response".into(),
                    Err(e) => self.status = format!("run failed: {e}"),
                }
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

        match key.code {
            KeyCode::Esc => {
                self.form = None;
                self.status = "cancelled".into();
            }
            KeyCode::Enter => self.advance_or_submit_form().await?,
            // Tab on the repository field with a suggestion fills the top match;
            // otherwise (and for Down) it advances to the next field.
            KeyCode::Tab if !self.repo_suggestions().is_empty() => {
                if let Some(top) = self.repo_suggestions().into_iter().next() {
                    if let Some(form) = self.form.as_mut() {
                        if let Some(field) = form.fields.get_mut(form.current) {
                            field.set_value(top);
                        }
                    }
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(form) = self.form.as_mut() {
                    form.current = (form.current + 1).min(form.fields.len().saturating_sub(1));
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(form) = self.form.as_mut() {
                    form.current = form.current.saturating_sub(1);
                }
            }
            KeyCode::Backspace => {
                if let Some(form) = self.form.as_mut() {
                    if let Some(field) = form.fields.get_mut(form.current) {
                        field.backspace();
                    }
                }
            }
            KeyCode::Delete => {
                if let Some(form) = self.form.as_mut() {
                    if let Some(field) = form.fields.get_mut(form.current) {
                        field.delete();
                    }
                }
            }
            KeyCode::Left => {
                if let Some(form) = self.form.as_mut() {
                    if let Some(field) = form.fields.get_mut(form.current) {
                        field.move_left();
                    }
                }
            }
            KeyCode::Right => {
                if let Some(form) = self.form.as_mut() {
                    if let Some(field) = form.fields.get_mut(form.current) {
                        field.move_right();
                    }
                }
            }
            KeyCode::Home => {
                if let Some(form) = self.form.as_mut() {
                    if let Some(field) = form.fields.get_mut(form.current) {
                        field.move_home();
                    }
                }
            }
            KeyCode::End => {
                if let Some(form) = self.form.as_mut() {
                    if let Some(field) = form.fields.get_mut(form.current) {
                        field.move_end();
                    }
                }
            }
            KeyCode::Char(c) => {
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                    if let Some(form) = self.form.as_mut() {
                        if let Some(field) = form.fields.get_mut(form.current) {
                            field.insert_char(c);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn advance_or_submit_form(&mut self) -> Result<()> {
        let should_submit = self
            .form
            .as_ref()
            .map(|f| f.current + 1 >= f.fields.len())
            .unwrap_or(false);
        if !should_submit {
            if let Some(form) = self.form.as_mut() {
                form.current += 1;
            }
            return Ok(());
        }
        self.submit_form().await
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
                let cmd = Command::AddProject {
                    name: form.get("name"),
                    repo_path: form.get("repo_path"),
                    default_branch: form.get("branch"),
                    main_agent_cmd: form.get("main_cmd"),
                    plan_agent_cmd: form.get("plan_cmd"),
                    work_agent_cmd: form.get("work_cmd"),
                    review_agent_cmd: form.opt("review_cmd"),
                    completion_policy: None,
                    plan_gate_timeout_min: None,
                    completion_soft_timeout_min: None,
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
                let Some(completion_policy) = parse_choice(
                    &form,
                    "completion",
                    "manual, soft, or auto",
                    CompletionPolicy::from_str,
                    &mut self.status,
                ) else {
                    return Ok(());
                };
                let Some(plan_gate_timeout_min) = parse_i64(&form, "plan_gate", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(completion_soft_timeout_min) =
                    parse_i64(&form, "complete_gate", &mut self.status)
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
                let Some(schedule_interval_min) =
                    parse_opt_i64(&form, "schedule_min", &mut self.status)
                else {
                    return Ok(());
                };
                let Some(merge_mode) = parse_choice(
                    &form,
                    "merge_mode",
                    "local or pr",
                    MergeMode::from_str,
                    &mut self.status,
                ) else {
                    return Ok(());
                };
                let Some(deepsleep_interval_days) =
                    parse_i64(&form, "deepsleep_days", &mut self.status)
                else {
                    return Ok(());
                };
                self.req_ok(
                    Command::UpdateProject {
                        project_id,
                        name: form.get("name"),
                        repo_path: form.get("repo_path"),
                        default_branch: form.get("branch"),
                        main_agent_cmd: form.get("main_cmd"),
                        plan_agent_cmd: form.get("plan_cmd"),
                        work_agent_cmd: form.get("work_cmd"),
                        review_agent_cmd: form.opt("review_cmd"),
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
                        skill_path: form.opt("skill_path"),
                        deepsleep_interval_days,
                    },
                    "project update",
                )
                .await;
                self.form = None;
                self.refresh_projects().await?;
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
                let Some(routine_type) = parse_routine_type(&form, &mut self.status) else {
                    return Ok(());
                };
                let Some(enabled) = parse_bool(&form, "enabled", &mut self.status) else {
                    return Ok(());
                };
                self.submit_create(
                    Command::CreateRoutine {
                        project_id,
                        name: form.get("name"),
                        routine_type,
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
                let Some(routine_type) = parse_routine_type(&form, &mut self.status) else {
                    return Ok(());
                };
                let Some(enabled) = parse_bool(&form, "enabled", &mut self.status) else {
                    return Ok(());
                };
                self.req_ok(
                    Command::UpdateRoutine {
                        routine_id,
                        name: form.get("name"),
                        routine_type,
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
            FormKind::Issue => {
                let Some(project_id) = self.selected_project_id() else {
                    self.status = "select a project first".into();
                    return Ok(());
                };
                self.submit_create(
                    Command::AddIssue {
                        project_id,
                        title: form.get("title"),
                        description: form.opt("description"),
                    },
                    "issue",
                )
                .await?;
                self.refresh_issues().await?;
            }
            FormKind::Subtask => {
                let Some(issue_id) = self.selected_issue_id() else {
                    self.status = "select an issue first".into();
                    return Ok(());
                };
                let Some(ord) = parse_i64(&form, "ord", &mut self.status) else {
                    return Ok(());
                };
                self.submit_create(
                    Command::AddSubtask {
                        issue_id,
                        ord,
                        text: form.get("text"),
                    },
                    "subtask",
                )
                .await?;
                self.refresh_detail().await?;
            }
            FormKind::Steering => {
                let Some(issue_id) = self.selected_issue_id() else {
                    self.status = "select an issue first".into();
                    return Ok(());
                };
                self.submit_create(
                    Command::AddSteering {
                        issue_id,
                        source: auwsx_core::steering::SteeringSource::Human,
                        note: form.get("note"),
                    },
                    "steering",
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
        // Entering a view freshens exactly what it shows.
        match v {
            View::Issue => self.refresh_detail().await?,
            View::Backlog => self.refresh_backlog().await?,
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
        if let Some(TreeItem::Issue { id, .. }) = self.selected_tree_item() {
            if let Some(idx) = self.issues().iter().position(|i| i.id == id) {
                self.issue_sel = idx;
            }
            self.refresh_detail().await?;
        } else {
            self.refresh_activity().await?;
        }
        Ok(())
    }

    fn clamp_tree(&mut self) {
        let len = self.tree_rows().len();
        step(&mut self.tree_sel, 0, len);
    }

    async fn drill(&mut self) -> Result<()> {
        if self.view != View::Overview {
            return Ok(());
        }
        match self.selected_tree_item() {
            // Enter on a project header toggles its children.
            Some(TreeItem::Project(pid)) => {
                if self.expanded.contains(&pid) {
                    self.expanded.remove(&pid);
                } else {
                    self.expanded.insert(pid);
                }
                self.clamp_tree();
                self.sync_active_project();
            }
            // Enter on an issue opens the detail view.
            Some(TreeItem::Issue { .. }) => {
                if self.selected_issue_id().is_some() {
                    self.set_view(View::Issue).await?;
                }
            }
            _ => {}
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
            Event::IssueStatus { .. }
            | Event::FindingAdded { .. }
            | Event::SteeringAdded { .. } => {
                self.refresh_issues().await?;
                self.refresh_detail().await?;
                self.refresh_activity().await?;
            }
            Event::BacklogChanged { .. } => {
                self.refresh_backlog().await?;
                self.refresh_activity().await?;
            }
            Event::SchedulerTick { project_id } => {
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
            format!("steering #{steering_id} on issue #{issue_id}")
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
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<KeyEvent>();
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(CEvent::Key(k)) if k.kind == KeyEventKind::Press => {
                if key_tx.send(k).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
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

    let mut tick = tokio::time::interval(Duration::from_secs(2));

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        tokio::select! {
            maybe_key = key_rx.recv() => {
                let Some(key) = maybe_key else { break }; // reader thread gone
                if let Some(action) = input::map_key(app.view, key) {
                    if app.form.is_some() {
                        app.handle_form_key(key).await?;
                    } else if app.apply(action).await? {
                        break;
                    }
                } else if app.form.is_some() {
                    app.handle_form_key(key).await?;
                }
            }
            ev = next_event(&mut events) => match ev {
                Some(Ok(e)) => app.on_event(e).await?,
                _ => {
                    // Stream closed or errored: drop to poll-only mode.
                    events = None;
                    app.connected = false;
                }
            },
            repos = drain_scan(&mut repo_scan) => {
                app.scanned_repos = repos;
                repo_scan = None; // consumed; never poll the finished handle again
            }
            _ = tick.tick() => { /* periodic redraw for time-based fields */ }
        }
    }
    Ok(())
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

    #[test]
    fn view_step_forward_one() {
        assert_eq!(View::Overview.step(1), View::Issue);
    }

    #[test]
    fn view_step_wraps_forward() {
        assert_eq!(View::Config.step(1), View::Overview);
    }

    #[test]
    fn view_step_wraps_backward() {
        assert_eq!(View::Overview.step(-1), View::Config);
    }

    #[test]
    fn view_step_forward_two() {
        assert_eq!(View::Backlog.step(2), View::Config);
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

    // --- project config validation ---------------------------------------

    fn test_project() -> Project {
        Project {
            id: 42,
            name: "demo".into(),
            repo_path: "/repo".into(),
            default_branch: "main".into(),
            main_agent_cmd: "main-agent".into(),
            plan_agent_cmd: "plan-agent".into(),
            work_agent_cmd: "work-agent".into(),
            review_agent_cmd: Some("review-agent".into()),
            completion_policy: CompletionPolicy::Manual,
            completion_soft_timeout_min: 30,
            plan_gate_timeout_min: 10,
            iteration_timeout_min: 60,
            main_job_timeout_min: 120,
            review_max_rounds: 2,
            conflict_max_attempts: 3,
            max_concurrency: 4,
            schedule_interval_min: Some(15),
            merge_mode: MergeMode::Local,
            skill_path: Some("skills".into()),
            deepsleep_interval_days: 7,
            last_deepsleep_at: None,
            created_at: 1,
        }
    }

    fn optional_test_field(key: &'static str, label: &'static str, value: &str) -> FormField {
        FormField {
            key,
            label,
            value: value.into(),
            cursor: value.chars().count(),
            optional: true,
        }
    }

    fn project_config_form_with(key: &'static str, value: &str) -> Form {
        let mut form = Form {
            kind: FormKind::ProjectConfig,
            title: "Project config",
            fields: vec![
                test_field("name", "Name", "demo"),
                test_field("repo_path", "Repository", "/repo"),
                test_field("branch", "Default branch", "main"),
                test_field("main_cmd", "Main command", "main-agent"),
                test_field("plan_cmd", "Plan command", "plan-agent"),
                test_field("work_cmd", "Work command", "work-agent"),
                optional_test_field("review_cmd", "Review command", "review-agent"),
                test_field("completion", "Completion policy", "manual"),
                test_field("plan_gate", "Plan gate timeout", "10"),
                test_field("complete_gate", "Completion timeout", "30"),
                test_field("iter_timeout", "Iteration timeout", "60"),
                test_field("main_job_timeout", "Main job timeout", "120"),
                test_field("review_rounds", "Review rounds", "2"),
                test_field("conflict_attempts", "Conflict attempts", "3"),
                test_field("concurrency", "Concurrency", "4"),
                optional_test_field("schedule_min", "Schedule interval", "15"),
                test_field("merge_mode", "Merge mode", "local"),
                optional_test_field("skill_path", "Skills path", "skills"),
                test_field("deepsleep_days", "Deepsleep interval", "7"),
            ],
            current: 0,
        };
        form.fields
            .iter_mut()
            .find(|field| field.key == key)
            .expect("test field exists")
            .value = value.into();
        form
    }

    async fn submitted_project_config_with(key: &'static str, value: &str) -> App {
        let mut app = App::new(std::path::PathBuf::from(
            "target/nonexistent-auwsx-test.sock",
        ));
        app.projects = vec![test_project()];
        app.form = Some(project_config_form_with(key, value));
        app.submit_form().await.unwrap();
        app
    }

    #[tokio::test]
    async fn given_invalid_completion_policy_when_submitting_project_config_then_status_uses_label()
    {
        let app = submitted_project_config_with("completion", "sometimes").await;

        assert_eq!(
            app.status,
            "Completion policy must be manual, soft, or auto"
        );
    }

    #[tokio::test]
    async fn given_invalid_merge_mode_when_submitting_project_config_then_status_uses_label() {
        let app = submitted_project_config_with("merge_mode", "squash").await;

        assert_eq!(app.status, "Merge mode must be local or pr");
    }

    #[tokio::test]
    async fn given_invalid_project_config_choice_when_submitting_then_form_stays_open() {
        let app = submitted_project_config_with("completion", "sometimes").await;

        assert!(app.form.is_some());
    }

    // --- App::repo_suggestions -------------------------------------------

    fn test_app() -> App {
        App::new(std::path::PathBuf::from("/tmp/test.sock"))
    }

    fn repo_field(value: &str) -> FormField {
        FormField::new("repo_path", "Repository", value, false)
    }

    fn form_app_with_field(field: FormField) -> App {
        let mut app = test_app();
        app.form = Some(Form {
            kind: FormKind::Backlog,
            title: "t",
            fields: vec![field],
            current: 0,
        });
        app
    }

    fn form_field(app: &App) -> &FormField {
        &app.form.as_ref().unwrap().fields[0]
    }

    fn test_field(key: &'static str, label: &'static str, value: &str) -> FormField {
        FormField::new(key, label, value, false)
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
            fields: vec![test_field("name", "Name", "foo")],
            current: 0,
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
            fields: vec![test_field("name", "Name", "x"), repo_field("foo")],
            current: 1,
        });
        assert!(!app.repo_suggestions().is_empty());
    }

    #[test]
    fn given_repo_field_label_differs_when_repo_suggestions_then_uses_field_key() {
        let mut app = test_app();
        app.scanned_repos = vec!["~/foo".to_string()];
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![test_field("repo_path", "Source directory", "foo")],
            current: 0,
        });
        assert_eq!(app.repo_suggestions(), vec!["~/foo".to_string()]);
    }

    #[test]
    fn given_project_config_form_on_repo_field_when_repo_suggestions_then_returns_matches() {
        let mut app = test_app();
        app.scanned_repos = vec!["~/foo".to_string()];
        app.form = Some(Form {
            kind: FormKind::ProjectConfig,
            title: "t",
            fields: vec![repo_field("foo")],
            current: 0,
        });
        assert_eq!(app.repo_suggestions(), vec!["~/foo".to_string()]);
    }

    #[test]
    fn given_project_form_with_unfocused_repo_field_when_repo_suggestions_then_empty() {
        let mut app = test_app();
        app.scanned_repos = vec!["~/foo".to_string()];
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![repo_field("foo"), test_field("name", "Name", "")],
            current: 1,
        });
        assert!(app.repo_suggestions().is_empty());
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
        });
        assert_eq!(app.repo_suggestions().len(), 8);
    }

    #[tokio::test]
    async fn given_cursor_in_middle_when_char_typed_then_inserts_at_cursor() {
        let mut field = FormField::new("text", "text", "ab", false);
        field.cursor = 1;
        let mut app = form_app_with_field(field);

        app.handle_form_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .await
            .unwrap();

        let field = form_field(&app);
        assert_eq!(field.value, "aXb");
        assert_eq!(field.cursor, 2);
    }

    #[tokio::test]
    async fn given_cursor_after_middle_char_when_backspace_then_removes_left_char() {
        let mut field = FormField::new("text", "text", "abc", false);
        field.cursor = 2;
        let mut app = form_app_with_field(field);

        app.handle_form_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
            .await
            .unwrap();

        let field = form_field(&app);
        assert_eq!(field.value, "ac");
        assert_eq!(field.cursor, 1);
    }

    #[tokio::test]
    async fn given_cursor_before_middle_char_when_delete_then_removes_right_char() {
        let mut field = FormField::new("text", "text", "abc", false);
        field.cursor = 1;
        let mut app = form_app_with_field(field);

        app.handle_form_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE))
            .await
            .unwrap();

        let field = form_field(&app);
        assert_eq!(field.value, "ac");
        assert_eq!(field.cursor, 1);
    }

    #[test]
    fn given_stale_cursor_when_inserting_utf8_then_clamps_to_char_end() {
        let mut field = FormField::new("text", "text", "éa", false);
        field.cursor = 99;

        field.insert_char('中');

        assert_eq!(field.value, "éa中");
        assert_eq!(field.cursor, 3);
    }

    #[tokio::test]
    async fn given_repo_completion_when_tab_then_cursor_moves_to_suggestion_end() {
        let mut app = test_app();
        app.scanned_repos = vec!["~/foo".to_string()];
        let mut field = repo_field("foo");
        field.cursor = 1;
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![field],
            current: 0,
        });

        app.handle_form_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .unwrap();

        let field = form_field(&app);
        assert_eq!(field.value, "~/foo");
        assert_eq!(field.cursor, 5);
    }
}

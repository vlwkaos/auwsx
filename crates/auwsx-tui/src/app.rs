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
use std::collections::VecDeque;
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

/// Which pane has the cursor in the Overview view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Projects,
    Issues,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeItem {
    Project,
    RoutinesRoot,
    Routine(i64),
    BacklogRoot,
    Backlog(i64),
    IssuesRoot,
    Issue(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub item: TreeItem,
    pub label: String,
    pub depth: usize,
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
    pub label: &'static str,
    pub value: String,
    pub optional: bool,
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
                field("name", "", false),
                field("repo_path", &repo, false),
                field("branch", "main", false),
                field("main_cmd", &codex, false),
                field("plan_cmd", &codex, false),
                field("work_cmd", &codex, false),
                field("review_cmd", &codex, true),
            ],
            current: 0,
        }
    }

    fn project_config(project: &Project) -> Self {
        Self {
            kind: FormKind::ProjectConfig,
            title: "Project config",
            fields: vec![
                field("name", &project.name, false),
                field("repo_path", &project.repo_path, false),
                field("branch", &project.default_branch, false),
                field("main_cmd", &project.main_agent_cmd, false),
                field("plan_cmd", &project.plan_agent_cmd, false),
                field("work_cmd", &project.work_agent_cmd, false),
                field(
                    "review_cmd",
                    project.review_agent_cmd.as_deref().unwrap_or(""),
                    true,
                ),
                field("completion", project.completion_policy.as_str(), false),
                field(
                    "plan_gate",
                    &project.plan_gate_timeout_min.to_string(),
                    false,
                ),
                field(
                    "complete_gate",
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
                    "schedule_min",
                    &project
                        .schedule_interval_min
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    true,
                ),
                field("merge_mode", project.merge_mode.as_str(), false),
                field(
                    "skill_path",
                    project.skill_path.as_deref().unwrap_or(""),
                    true,
                ),
                field(
                    "deepsleep_days",
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
            .find(|f| f.label == label)
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
}

fn field(label: &'static str, value: &str, optional: bool) -> FormField {
    FormField {
        label,
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

fn parse_opt_i64(form: &Form, label: &'static str, status: &mut String) -> Option<Option<i64>> {
    let raw = form.get(label);
    if raw.is_empty() {
        return Some(None);
    }
    match raw.parse::<i64>() {
        Ok(value) => Some(Some(value)),
        Err(_) => {
            *status = format!("{label} must be blank or an integer");
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

fn parse_routine_type(form: &Form, status: &mut String) -> Option<RoutineType> {
    match RoutineType::from_str(&form.get("type")) {
        Some(value) => Some(value),
        None => {
            *status = "type must be report, idea, or knowledge".into();
            None
        }
    }
}

pub struct App {
    pub socket: PathBuf,
    pub view: View,
    pub pane: Pane,

    pub projects: Vec<Project>,
    pub proj_sel: usize,
    pub issues: Vec<Issue>,
    pub issue_sel: usize,
    pub backlog: Vec<BacklogItem>,
    pub backlog_sel: usize,
    pub routines: Vec<Routine>,
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
}

const LOG_CAP: usize = 500;

impl App {
    pub fn new(socket: PathBuf) -> Self {
        App {
            socket,
            view: View::Overview,
            pane: Pane::Projects,
            projects: Vec::new(),
            proj_sel: 0,
            issues: Vec::new(),
            issue_sel: 0,
            backlog: Vec::new(),
            backlog_sel: 0,
            routines: Vec::new(),
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
        }
    }

    /// The currently-selected project id (if any project exists).
    pub fn selected_project_id(&self) -> Option<i64> {
        self.projects.get(self.proj_sel).map(|p| p.id)
    }

    /// The currently-selected issue id (if any).
    pub fn selected_issue_id(&self) -> Option<i64> {
        match self.selected_tree_item() {
            Some(TreeItem::Issue(id)) => Some(id),
            _ => self.issues.get(self.issue_sel).map(|i| i.id),
        }
    }

    fn selected_backlog_id(&self) -> Option<i64> {
        match self.selected_tree_item() {
            Some(TreeItem::Backlog(id)) => Some(id),
            _ => self.backlog.get(self.backlog_sel).map(|b| b.id),
        }
    }

    pub fn tree_rows(&self) -> Vec<TreeRow> {
        let project_label = self
            .projects
            .get(self.proj_sel)
            .map(|p| format!("PROJECT  {}", p.name))
            .unwrap_or_else(|| "PROJECT  (none)".to_string());
        let mut rows = vec![TreeRow {
            item: TreeItem::Project,
            label: project_label,
            depth: 0,
        }];
        rows.push(TreeRow {
            item: TreeItem::RoutinesRoot,
            label: format!("ROUTINES  {}", self.routines.len()),
            depth: 0,
        });
        for r in &self.routines {
            rows.push(TreeRow {
                item: TreeItem::Routine(r.id),
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
            item: TreeItem::BacklogRoot,
            label: format!("BACKLOG   {}", self.backlog.len()),
            depth: 0,
        });
        for b in &self.backlog {
            let consumed = if b.consumed_issue_id.is_some() {
                "*"
            } else {
                " "
            };
            rows.push(TreeRow {
                item: TreeItem::Backlog(b.id),
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
            item: TreeItem::IssuesRoot,
            label: format!("ISSUES    {}", self.issues.len()),
            depth: 0,
        });
        for i in &self.issues {
            rows.push(TreeRow {
                item: TreeItem::Issue(i.id),
                label: format!("#{: <3} {:<13} {}", i.id, i.status.as_str(), i.title),
                depth: 2,
            });
        }
        rows
    }

    pub fn selected_tree_item(&self) -> Option<TreeItem> {
        self.tree_rows().get(self.tree_sel).map(|r| r.item.clone())
    }

    pub fn selected_routine(&self) -> Option<&Routine> {
        match self.selected_tree_item()? {
            TreeItem::Routine(id) => self.routines.iter().find(|r| r.id == id),
            _ => None,
        }
    }

    pub fn selected_backlog(&self) -> Option<&BacklogItem> {
        match self.selected_tree_item()? {
            TreeItem::Backlog(id) => self.backlog.iter().find(|b| b.id == id),
            _ => None,
        }
    }

    pub fn selected_issue(&self) -> Option<&Issue> {
        match self.selected_tree_item()? {
            TreeItem::Issue(id) => self.issues.iter().find(|i| i.id == id),
            _ => self.issues.get(self.issue_sel),
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

    /// Full resync of every cached list for the current selection.
    async fn refresh_all(&mut self) -> Result<()> {
        self.refresh_projects().await?;
        self.refresh_routines().await?;
        self.refresh_issues().await?;
        self.refresh_backlog().await?;
        self.refresh_detail().await?;
        self.refresh_activity().await?;
        self.clamp_tree();
        Ok(())
    }

    async fn refresh_projects(&mut self) -> Result<()> {
        if let Response::Projects(ps) = self.req(Command::ListProjects).await? {
            self.projects = ps;
            if self.proj_sel >= self.projects.len() {
                self.proj_sel = self.projects.len().saturating_sub(1);
            }
        }
        Ok(())
    }

    async fn refresh_issues(&mut self) -> Result<()> {
        let Some(pid) = self.selected_project_id() else {
            self.issues.clear();
            return Ok(());
        };
        if let Response::Issues(mut is) = self
            .req(Command::ListIssues {
                project_id: pid,
                status: None,
            })
            .await?
        {
            // Stable, readable order: oldest first (creation order == id order).
            is.sort_by_key(|i| i.id);
            self.issues = is;
            if self.issue_sel >= self.issues.len() {
                self.issue_sel = self.issues.len().saturating_sub(1);
            }
        }
        Ok(())
    }

    async fn refresh_backlog(&mut self) -> Result<()> {
        let Some(pid) = self.selected_project_id() else {
            self.backlog.clear();
            return Ok(());
        };
        if let Response::Backlog(items) = self
            .req(Command::ListBacklog {
                project_id: pid,
                approval: None,
            })
            .await?
        {
            self.backlog = items;
            if self.backlog_sel >= self.backlog.len() {
                self.backlog_sel = self.backlog.len().saturating_sub(1);
            }
        }
        Ok(())
    }

    async fn refresh_routines(&mut self) -> Result<()> {
        let Some(pid) = self.selected_project_id() else {
            self.routines.clear();
            return Ok(());
        };
        if let Response::Routines(items) =
            self.req(Command::ListRoutines { project_id: pid }).await?
        {
            self.routines = items;
        }
        Ok(())
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
                Some(TreeItem::Backlog(item_id)) => {
                    if let Some(item) = self.backlog.iter().find(|b| b.id == item_id) {
                        if item.consumed_issue_id.is_some() {
                            self.status = "consumed backlog cannot be edited".into();
                        } else {
                            self.form = Some(Form::backlog_edit(item));
                        }
                    }
                }
                Some(TreeItem::Routine(routine_id)) => {
                    if let Some(routine) = self.routines.iter().find(|r| r.id == routine_id) {
                        self.form = Some(Form::routine_edit(routine));
                    }
                }
                _ => self.status = "select a backlog item or routine to edit".into(),
            },
            Action::NewBacklog => {
                if matches!(
                    self.selected_tree_item(),
                    Some(TreeItem::RoutinesRoot | TreeItem::Routine(_))
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
            Some(TreeItem::Project) => {
                if let Some(project_id) = self.selected_project_id() {
                    self.req_ok(Command::RunSchedulerOnce { project_id }, "scheduler tick")
                        .await;
                }
            }
            Some(TreeItem::Backlog(item_id)) => {
                match self.req(Command::RunBacklogNow { item_id }).await {
                    Ok(Response::RanIssue { issue_id }) => {
                        self.status = format!("running issue #{issue_id}");
                    }
                    Ok(Response::Err { message }) => self.status = format!("run failed: {message}"),
                    Ok(_) => self.status = "run failed: unexpected response".into(),
                    Err(e) => self.status = format!("run failed: {e}"),
                }
            }
            Some(TreeItem::Issue(issue_id)) => {
                match self.req(Command::RunIssueNow { issue_id }).await {
                    Ok(Response::RanIssue { issue_id }) => {
                        self.status = format!("running issue #{issue_id}");
                    }
                    Ok(Response::Err { message }) => self.status = format!("run failed: {message}"),
                    Ok(_) => self.status = "run failed: unexpected response".into(),
                    Err(e) => self.status = format!("run failed: {e}"),
                }
            }
            Some(TreeItem::Routine(routine_id)) => {
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
                        field.value.pop();
                    }
                }
            }
            KeyCode::Char(c) => {
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                    if let Some(form) = self.form.as_mut() {
                        if let Some(field) = form.fields.get_mut(form.current) {
                            field.value.push(c);
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
                self.refresh_issues().await?;
                self.refresh_backlog().await?;
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
                let Some(merge_mode) = MergeMode::from_str(&form.get("merge_mode")) else {
                    self.status = "merge_mode must be local or pr".into();
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
        if let Some(TreeItem::Issue(id)) = self.selected_tree_item() {
            if let Some(idx) = self.issues.iter().position(|i| i.id == id) {
                self.issue_sel = idx;
            }
            self.refresh_detail().await?;
        }
        Ok(())
    }

    fn clamp_tree(&mut self) {
        let len = self.tree_rows().len();
        step(&mut self.tree_sel, 0, len);
    }

    async fn drill(&mut self) -> Result<()> {
        if self.view == View::Overview {
            if self.pane == Pane::Projects {
                // Drill from a project into its issues pane.
                self.pane = Pane::Issues;
            } else if self.selected_issue_id().is_some() {
                self.set_view(View::Issue).await?;
            }
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
            _ = tick.tick() => { /* periodic redraw for time-based fields */ }
        }
    }
    Ok(())
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
}

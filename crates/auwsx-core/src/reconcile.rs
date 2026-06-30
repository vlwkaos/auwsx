//! Project reconciliation diagnostics and safe recovery actions.
//!
//! This is the deterministic layer before merge release. It classifies branch,
//! worktree, and issue state so auwsx can auto-apply only high-confidence fixes
//! and route ambiguous cases to a queued main-job agent.

use crate::db::issues::Issue;
use crate::db::projects::Project;
use crate::state::IssueStatus;
use crate::worktree::{branch_for_issue, list_issue_worktrees, WorktreeHandle};
use crate::Result;
use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileDiagnosis {
    SafeToMerge,
    RepresentedInMain,
    StaleNoDiffBranch,
    MergeConflict,
    DirtyMainBlocked,
    RestoreDirtyBlocked,
    MissingBranch,
    MissingWorktree,
    OrphanWorktree,
    FailedWithWorktree,
    Running,
    Unknown,
}

impl ReconcileDiagnosis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SafeToMerge => "safe_to_merge",
            Self::RepresentedInMain => "represented_in_main",
            Self::StaleNoDiffBranch => "stale_no_diff_branch",
            Self::MergeConflict => "merge_conflict",
            Self::DirtyMainBlocked => "dirty_main_blocked",
            Self::RestoreDirtyBlocked => "restore_dirty_blocked",
            Self::MissingBranch => "missing_branch",
            Self::MissingWorktree => "missing_worktree",
            Self::OrphanWorktree => "orphan_worktree",
            Self::FailedWithWorktree => "failed_with_worktree",
            Self::Running => "running",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileActionKind {
    None,
    MarkDone,
    CleanupWorktree,
    ApplyMerge,
    PruneOrphanWorktree,
    RetryIssue,
    ManualRequired,
    QueueAgenticReconcile,
}

impl ReconcileActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::MarkDone => "mark_done",
            Self::CleanupWorktree => "cleanup_worktree",
            Self::ApplyMerge => "apply_merge",
            Self::PruneOrphanWorktree => "prune_orphan_worktree",
            Self::RetryIssue => "retry_issue",
            Self::ManualRequired => "manual_required",
            Self::QueueAgenticReconcile => "queue_agentic_reconcile",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileIssueReport {
    pub issue_id: i64,
    pub status: IssueStatus,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub diagnosis: ReconcileDiagnosis,
    pub confidence: u8,
    pub proposed_action: ReconcileActionKind,
    pub blocking_reason: Option<String>,
    pub manual_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileOrphanReport {
    pub issue_id: i64,
    pub branch: String,
    pub path: String,
    pub diagnosis: ReconcileDiagnosis,
    pub proposed_action: ReconcileActionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectReconcileReport {
    pub project_id: i64,
    pub dry_run: bool,
    pub issues: Vec<ReconcileIssueReport>,
    pub orphans: Vec<ReconcileOrphanReport>,
    pub safe_count: usize,
    pub manual_count: usize,
    pub agentic_count: usize,
    pub applied_count: usize,
    pub queued_main_job_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileDiagnosisCounts {
    pub safe: usize,
    pub represented: usize,
    pub conflict: usize,
    pub stale: usize,
    pub unknown: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReconcileProposal {
    pub schema_version: u8,
    pub kind: String,
    pub proposal: Option<String>,
    pub rationale: Option<String>,
    pub actions: Vec<AgentReconcileAction>,
    pub verification: Option<serde_json::Value>,
    pub risk: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReconcileAction {
    #[serde(alias = "name", alias = "type")]
    pub action: ReconcileActionKind,
    pub issue_id: Option<i64>,
    pub rationale: Option<String>,
    pub command: Option<String>,
}

impl ProjectReconcileReport {
    pub fn empty(project_id: i64, dry_run: bool) -> Self {
        Self {
            project_id,
            dry_run,
            issues: Vec::new(),
            orphans: Vec::new(),
            safe_count: 0,
            manual_count: 0,
            agentic_count: 0,
            applied_count: 0,
            queued_main_job_id: None,
        }
    }

    pub fn refresh_counts(&mut self) {
        self.safe_count = self.safe_action_count();
        self.manual_count = self.manual_action_count();
        self.agentic_count = self.agentic_action_count();
    }

    pub fn diagnosis_counts(&self) -> ReconcileDiagnosisCounts {
        let mut counts = ReconcileDiagnosisCounts::default();
        for issue in &self.issues {
            match issue.diagnosis {
                ReconcileDiagnosis::SafeToMerge => counts.safe += 1,
                ReconcileDiagnosis::RepresentedInMain => counts.represented += 1,
                ReconcileDiagnosis::MergeConflict
                | ReconcileDiagnosis::DirtyMainBlocked
                | ReconcileDiagnosis::RestoreDirtyBlocked => counts.conflict += 1,
                ReconcileDiagnosis::StaleNoDiffBranch => counts.stale += 1,
                ReconcileDiagnosis::Unknown => counts.unknown += 1,
                _ => {}
            }
        }
        counts
    }

    fn safe_action_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| {
                matches!(
                    issue.proposed_action,
                    ReconcileActionKind::MarkDone
                        | ReconcileActionKind::CleanupWorktree
                        | ReconcileActionKind::ApplyMerge
                )
            })
            .count()
            + self.orphans.len()
    }

    fn manual_action_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.proposed_action == ReconcileActionKind::ManualRequired)
            .count()
    }

    fn agentic_action_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.proposed_action == ReconcileActionKind::QueueAgenticReconcile)
            .count()
    }
}

pub async fn diagnose_project(
    project: &Project,
    issues: &[Issue],
    running_issue_ids: &[i64],
    known_paths: &HashMap<i64, PathBuf>,
    dry_run: bool,
) -> Result<ProjectReconcileReport> {
    let mut report = ProjectReconcileReport::empty(project.id, dry_run);
    let repo_path = Path::new(&project.repo_path);
    let running: std::collections::HashSet<i64> = running_issue_ids.iter().copied().collect();

    for issue in issues {
        report.issues.push(diagnose_issue(
            repo_path,
            issue,
            running.contains(&issue.id),
        )?);
    }

    let worktrees = list_issue_worktrees(repo_path).await?;
    for worktree in worktrees {
        if known_paths
            .get(&worktree.issue_id)
            .is_some_and(|path| path == &worktree.handle.path)
        {
            continue;
        }
        report
            .orphans
            .push(orphan_report(worktree.issue_id, worktree.handle));
    }
    report.refresh_counts();
    Ok(report)
}

fn orphan_report(issue_id: i64, handle: WorktreeHandle) -> ReconcileOrphanReport {
    ReconcileOrphanReport {
        issue_id,
        branch: handle.branch,
        path: handle.path.display().to_string(),
        diagnosis: ReconcileDiagnosis::OrphanWorktree,
        proposed_action: ReconcileActionKind::PruneOrphanWorktree,
    }
}

fn diagnose_issue(repo_path: &Path, issue: &Issue, running: bool) -> Result<ReconcileIssueReport> {
    if running {
        return Ok(base_report(
            issue,
            ReconcileDiagnosis::Running,
            100,
            ReconcileActionKind::None,
            Some(format!("issue {} is currently running", issue.id)),
            None,
        ));
    }

    let Some(branch) = issue.branch.as_deref() else {
        return Ok(match issue.status {
            IssueStatus::ReadyToMerge | IssueStatus::Merging | IssueStatus::ConflictBlocked => {
                base_report(
                    issue,
                    ReconcileDiagnosis::MissingBranch,
                    100,
                    ReconcileActionKind::ManualRequired,
                    Some("issue has no branch for merge/recovery".to_string()),
                    Some(format!("auwsx issue get {}", issue.id)),
                )
            }
            IssueStatus::Failed if issue.worktree_path.is_none() => base_report(
                issue,
                ReconcileDiagnosis::Unknown,
                50,
                ReconcileActionKind::QueueAgenticReconcile,
                Some("failed issue has no branch or worktree to inspect".to_string()),
                Some(format!("auwsx issue retry {}", issue.id)),
            ),
            _ => base_report(
                issue,
                ReconcileDiagnosis::Unknown,
                50,
                ReconcileActionKind::None,
                None,
                None,
            ),
        });
    };

    let expected_branch = branch_for_issue(issue.id);
    if branch != expected_branch
        && matches!(
            issue.status,
            IssueStatus::ReadyToMerge
                | IssueStatus::Merging
                | IssueStatus::ConflictBlocked
                | IssueStatus::Failed
        )
    {
        return Ok(base_report(
            issue,
            ReconcileDiagnosis::Unknown,
            100,
            ReconcileActionKind::ManualRequired,
            Some(format!(
                "branch {branch} is not the auwsx-managed branch {expected_branch}"
            )),
            Some(format!("auwsx issue get {}", issue.id)),
        ));
    }

    if !branch_exists(repo_path, branch)? {
        return Ok(base_report(
            issue,
            ReconcileDiagnosis::MissingBranch,
            100,
            ReconcileActionKind::ManualRequired,
            Some(format!("branch {branch} does not exist")),
            Some(format!("auwsx issue cleanup {}", issue.id)),
        ));
    }

    if is_merge_recovery_status(issue.status) && branch_is_ancestor_of_head(repo_path, branch)? {
        return Ok(base_report(
            issue,
            ReconcileDiagnosis::RepresentedInMain,
            100,
            ReconcileActionKind::MarkDone,
            Some(format!("branch {branch} is already contained in HEAD")),
            Some(format!("auwsx issue status {} DONE --force", issue.id)),
        ));
    }

    if is_merge_recovery_status(issue.status) && trees_equal(repo_path, "HEAD", branch)? {
        return Ok(base_report(
            issue,
            ReconcileDiagnosis::StaleNoDiffBranch,
            100,
            ReconcileActionKind::MarkDone,
            Some(format!("branch {branch} has the same tree as HEAD")),
            Some(format!("auwsx issue status {} DONE --force", issue.id)),
        ));
    }

    if matches!(
        issue.status,
        IssueStatus::ReadyToMerge | IssueStatus::Merging
    ) {
        let dirty_overlap = dirty_branch_overlap(repo_path, branch)?;
        if !dirty_overlap.is_empty() {
            return Ok(base_report(
                issue,
                ReconcileDiagnosis::RestoreDirtyBlocked,
                95,
                ReconcileActionKind::QueueAgenticReconcile,
                Some(format!(
                    "dirty primary worktree overlaps branch changes: {}",
                    dirty_overlap.join(", ")
                )),
                Some(format!("auwsx issue apply-merge {}", issue.id)),
            ));
        }
        let preflight = merge_preflight(repo_path, branch)?;
        if preflight.conflicts {
            return Ok(base_report(
                issue,
                ReconcileDiagnosis::MergeConflict,
                95,
                ReconcileActionKind::QueueAgenticReconcile,
                Some(preflight.summary),
                Some(format!("auwsx issue apply-merge {}", issue.id)),
            ));
        }
        return Ok(base_report(
            issue,
            ReconcileDiagnosis::SafeToMerge,
            90,
            ReconcileActionKind::ApplyMerge,
            Some(preflight.summary),
            Some(format!("auwsx issue merge {}", issue.id)),
        ));
    }

    if issue.status == IssueStatus::Failed && issue.worktree_path.is_some() {
        return Ok(base_report(
            issue,
            ReconcileDiagnosis::FailedWithWorktree,
            80,
            ReconcileActionKind::QueueAgenticReconcile,
            Some("failed issue still has worktree state to inspect".to_string()),
            Some(format!("auwsx issue retry {}", issue.id)),
        ));
    }

    Ok(base_report(
        issue,
        ReconcileDiagnosis::Unknown,
        50,
        ReconcileActionKind::QueueAgenticReconcile,
        Some("issue needs operator or agentic reconciliation".to_string()),
        Some(format!("auwsx issue get {}", issue.id)),
    ))
}

fn is_merge_recovery_status(status: IssueStatus) -> bool {
    matches!(
        status,
        IssueStatus::ReadyToMerge | IssueStatus::Merging | IssueStatus::ConflictBlocked
    )
}

fn base_report(
    issue: &Issue,
    diagnosis: ReconcileDiagnosis,
    confidence: u8,
    proposed_action: ReconcileActionKind,
    blocking_reason: Option<String>,
    manual_command: Option<String>,
) -> ReconcileIssueReport {
    ReconcileIssueReport {
        issue_id: issue.id,
        status: issue.status,
        branch: issue.branch.clone(),
        worktree_path: issue.worktree_path.clone(),
        diagnosis,
        confidence,
        proposed_action,
        blocking_reason,
        manual_command,
    }
}

struct MergePreflight {
    conflicts: bool,
    summary: String,
}

fn merge_preflight(repo_path: &Path, branch: &str) -> Result<MergePreflight> {
    let output = git(repo_path, &["merge-tree", "HEAD", branch])?;
    let text = command_text(&output);
    let conflicts = !output.status.success()
        || text.contains("CONFLICT")
        || text.contains("<<<<<<<")
        || text.contains("changed in both");
    Ok(MergePreflight {
        conflicts,
        summary: summarize(
            &text,
            if conflicts {
                "merge preflight conflict"
            } else {
                "merge preflight clean"
            },
        ),
    })
}

fn branch_exists(repo_path: &Path, branch: &str) -> Result<bool> {
    let output = git(
        repo_path,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    Ok(output.status.success())
}

fn branch_is_ancestor_of_head(repo_path: &Path, branch: &str) -> Result<bool> {
    let output = git(repo_path, &["merge-base", "--is-ancestor", branch, "HEAD"])?;
    Ok(output.status.success())
}

fn trees_equal(repo_path: &Path, left: &str, right: &str) -> Result<bool> {
    let output = git(repo_path, &["diff", "--quiet", left, right])?;
    Ok(output.status.success())
}

fn dirty_branch_overlap(repo_path: &Path, branch: &str) -> Result<Vec<String>> {
    let dirty = dirty_paths(repo_path)?;
    if dirty.is_empty() {
        return Ok(Vec::new());
    }
    let changed = branch_changed_paths(repo_path, branch)?;
    Ok(dirty
        .into_iter()
        .filter(|path| changed.contains(path))
        .collect())
}

fn dirty_paths(repo_path: &Path) -> Result<std::collections::HashSet<String>> {
    let output = git(
        repo_path,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    if !output.status.success() {
        bail!(
            "git status failed: {}",
            summarize(&command_text(&output), "status failed")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_status_path)
        .collect())
}

fn branch_changed_paths(
    repo_path: &Path,
    branch: &str,
) -> Result<std::collections::HashSet<String>> {
    let output = git(repo_path, &["diff", "--name-only", "HEAD", branch])?;
    if !output.status.success() {
        bail!(
            "git diff failed: {}",
            summarize(&command_text(&output), "diff failed")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn parse_status_path(line: &str) -> Option<String> {
    if line.len() < 4 {
        return None;
    }
    let path = line[3..].trim();
    if path.is_empty() {
        return None;
    }
    Some(
        path.rsplit_once(" -> ")
            .map(|(_, to)| to)
            .unwrap_or(path)
            .to_string(),
    )
}

fn git(repo_path: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .current_dir(repo_path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawning git {}", args.join(" ")))
}

fn command_text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn summarize(text: &str, fallback: &str) -> String {
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(8)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        fallback.to_string()
    } else {
        lines.join("; ")
    }
}

pub fn validate_agent_action(
    report: &ProjectReconcileReport,
    issue_id: i64,
    action: ReconcileActionKind,
) -> Result<()> {
    let Some(issue) = report
        .issues
        .iter()
        .find(|issue| issue.issue_id == issue_id)
    else {
        bail!("agent proposal references issue {issue_id}, which is absent from reconcile report");
    };
    match action {
        ReconcileActionKind::MarkDone => {
            if !matches!(
                issue.diagnosis,
                ReconcileDiagnosis::RepresentedInMain | ReconcileDiagnosis::StaleNoDiffBranch
            ) {
                bail!("mark_done requires represented_in_main or stale_no_diff_branch");
            }
        }
        ReconcileActionKind::CleanupWorktree => {
            if issue.branch.as_deref() != Some(&branch_for_issue(issue_id)) {
                bail!("cleanup_worktree requires the auwsx-managed issue branch");
            }
            if issue.worktree_path.is_none() {
                bail!("cleanup_worktree requires a recorded issue worktree");
            }
            if issue.status.is_terminal() || issue.status == IssueStatus::Failed {
                return Ok(());
            }
            bail!("cleanup_worktree requires a terminal or failed issue");
        }
        ReconcileActionKind::RetryIssue => {
            if issue.status != IssueStatus::Failed {
                bail!("retry_issue requires FAILED status");
            }
        }
        ReconcileActionKind::ApplyMerge => {
            if issue.diagnosis != ReconcileDiagnosis::SafeToMerge {
                bail!("apply_merge requires safe_to_merge diagnosis");
            }
        }
        _ => return Err(anyhow!("agent action {:?} is not applyable", action)),
    }
    Ok(())
}

pub fn parse_agent_proposal(text: &str) -> Result<AgentReconcileProposal> {
    let fenced = final_fenced_json(text)?;
    let proposal = serde_json::from_str::<AgentReconcileProposal>(fenced)
        .context("decoding final reconcile proposal JSON")?;
    validate_proposal_envelope(proposal)
}

fn validate_proposal_envelope(proposal: AgentReconcileProposal) -> Result<AgentReconcileProposal> {
    if proposal.schema_version != 1 {
        bail!(
            "unsupported reconcile proposal schema_version {}",
            proposal.schema_version
        );
    }
    if proposal.kind != "auwsx_reconcile_proposal" {
        bail!("proposal kind must be auwsx_reconcile_proposal");
    }
    Ok(proposal)
}

fn final_fenced_json(text: &str) -> Result<&str> {
    let marker = "```json";
    let start = text
        .rfind(marker)
        .ok_or_else(|| anyhow!("reconcile proposal must end with one final ```json block"))?;
    let body = &text[start + marker.len()..];
    let end = body
        .find("```")
        .ok_or_else(|| anyhow!("final reconcile proposal JSON fence is not closed"))?;
    if !body[end + "```".len()..].trim().is_empty() {
        bail!("reconcile proposal JSON fence must be the final output");
    }
    let prior = &text[..start];
    if prior.contains(marker) {
        bail!("reconcile proposal output contains multiple JSON fences");
    }
    let json = body[..end].trim();
    if json.is_empty() {
        bail!("final reconcile proposal JSON is empty");
    }
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::projects::{CompletionPolicy, MergeMode};
    use tempfile::TempDir;

    fn project(repo: &Path) -> Project {
        Project {
            id: 1,
            profile_id: 1,
            profile_order: 0,
            name: "demo".to_string(),
            repo_path: repo.display().to_string(),
            default_branch: "main".to_string(),
            arsenal_preset_name: None,
            main_agent_cmd: "true".to_string(),
            route_agent_cmd: "true".to_string(),
            plan_agent_cmd: "true".to_string(),
            work_agent_cmd: "true".to_string(),
            review_agent_cmd: None,
            main_agent_cmd_override: None,
            route_agent_cmd_override: None,
            plan_agent_cmd_override: None,
            work_agent_cmd_override: None,
            review_agent_cmd_override: None,
            completion_policy: CompletionPolicy::Manual,
            completion_soft_timeout_min: 60,
            plan_gate_timeout_min: 0,
            iteration_timeout_min: 60,
            main_job_timeout_min: 60,
            review_max_rounds: 1,
            conflict_max_attempts: 1,
            max_concurrency: 3,
            schedule_interval_min: None,
            schedule_cron: None,
            merge_mode: MergeMode::Local,
            skill_path: None,
            deepsleep_interval_days: 7,
            deepsleep_cron: None,
            last_deepsleep_at: None,
            created_at: 0,
        }
    }

    fn issue(id: i64, status: IssueStatus, branch: &str) -> Issue {
        Issue {
            id,
            project_id: 1,
            title: format!("issue {id}"),
            description: None,
            agent_summary: None,
            progress_report: None,
            result_report: None,
            status,
            branch: Some(branch.to_string()),
            worktree_path: None,
            agent_session: None,
            review_round: 0,
            conflict_attempts: 0,
            wait_until: None,
            absorbed_into_id: None,
            has_pending_steering: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn repo() -> Result<TempDir> {
        let dir = tempfile::tempdir()?;
        git(dir.path(), &["init", "-b", "main"])?;
        git(
            dir.path(),
            &["config", "user.email", "auwsx@example.invalid"],
        )?;
        git(dir.path(), &["config", "user.name", "auwsx test"])?;
        write_file(dir.path(), "app.txt", "base\n")?;
        git(dir.path(), &["add", "app.txt"])?;
        git(dir.path(), &["commit", "-m", "base"])?;
        Ok(dir)
    }

    fn write_file(repo: &Path, name: &str, text: &str) -> Result<()> {
        std::fs::write(repo.join(name), text).with_context(|| format!("writing {name}"))
    }

    fn commit(repo: &Path, msg: &str) -> Result<()> {
        git(repo, &["add", "."])?;
        git(repo, &["commit", "-m", msg])?;
        Ok(())
    }

    fn first_report(repo: &Path, issue: Issue) -> Result<ReconcileIssueReport> {
        let project = project(repo);
        let runtime = tokio::runtime::Runtime::new()?;
        let report = runtime.block_on(diagnose_project(
            &project,
            &[issue],
            &[],
            &HashMap::new(),
            true,
        ))?;
        report
            .issues
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("missing issue report"))
    }

    fn report_issue(
        issue_id: i64,
        status: IssueStatus,
        branch: Option<&str>,
        worktree_path: Option<&str>,
        diagnosis: ReconcileDiagnosis,
        proposed_action: ReconcileActionKind,
    ) -> ReconcileIssueReport {
        ReconcileIssueReport {
            issue_id,
            status,
            branch: branch.map(str::to_string),
            worktree_path: worktree_path.map(str::to_string),
            diagnosis,
            confidence: 80,
            proposed_action,
            blocking_reason: None,
            manual_command: None,
        }
    }

    #[test]
    fn given_branch_ancestor_when_diagnose_then_mark_done() -> Result<()> {
        let dir = repo()?;
        git(dir.path(), &["branch", "auwsx/issue-1"])?;

        let report = first_report(
            dir.path(),
            issue(1, IssueStatus::ReadyToMerge, "auwsx/issue-1"),
        )?;

        assert_eq!(report.diagnosis, ReconcileDiagnosis::RepresentedInMain);
        assert_eq!(report.proposed_action, ReconcileActionKind::MarkDone);
        Ok(())
    }

    #[test]
    fn given_non_merge_status_and_branch_ancestor_when_diagnose_then_not_mark_done() -> Result<()> {
        let dir = repo()?;
        git(dir.path(), &["branch", "auwsx/issue-6"])?;

        let report = first_report(dir.path(), issue(6, IssueStatus::Working, "auwsx/issue-6"))?;

        assert_ne!(report.proposed_action, ReconcileActionKind::MarkDone);
        Ok(())
    }

    #[test]
    fn given_no_diff_branch_when_diagnose_then_mark_done() -> Result<()> {
        let dir = repo()?;
        git(dir.path(), &["checkout", "-b", "auwsx/issue-2"])?;
        git(
            dir.path(),
            &["commit", "--allow-empty", "-m", "empty issue"],
        )?;
        git(dir.path(), &["checkout", "main"])?;

        let report = first_report(
            dir.path(),
            issue(2, IssueStatus::ReadyToMerge, "auwsx/issue-2"),
        )?;

        assert_eq!(report.diagnosis, ReconcileDiagnosis::StaleNoDiffBranch);
        assert_eq!(report.proposed_action, ReconcileActionKind::MarkDone);
        Ok(())
    }

    #[test]
    fn given_clean_ready_branch_when_diagnose_then_apply_merge() -> Result<()> {
        let dir = repo()?;
        git(dir.path(), &["checkout", "-b", "auwsx/issue-3"])?;
        write_file(dir.path(), "feature.txt", "feature\n")?;
        commit(dir.path(), "feature")?;
        git(dir.path(), &["checkout", "main"])?;

        let report = first_report(
            dir.path(),
            issue(3, IssueStatus::ReadyToMerge, "auwsx/issue-3"),
        )?;

        assert_eq!(report.diagnosis, ReconcileDiagnosis::SafeToMerge);
        assert_eq!(report.proposed_action, ReconcileActionKind::ApplyMerge);
        Ok(())
    }

    #[test]
    fn given_dirty_primary_overlap_when_diagnose_then_queue_agentic_reconcile() -> Result<()> {
        let dir = repo()?;
        git(dir.path(), &["checkout", "-b", "auwsx/issue-7"])?;
        write_file(dir.path(), "app.txt", "branch\n")?;
        commit(dir.path(), "branch edit")?;
        git(dir.path(), &["checkout", "main"])?;
        write_file(dir.path(), "app.txt", "dirty main\n")?;

        let report = first_report(
            dir.path(),
            issue(7, IssueStatus::ReadyToMerge, "auwsx/issue-7"),
        )?;

        assert_eq!(report.diagnosis, ReconcileDiagnosis::RestoreDirtyBlocked);
        assert_eq!(
            report.proposed_action,
            ReconcileActionKind::QueueAgenticReconcile
        );
        Ok(())
    }

    #[test]
    fn given_dirty_primary_non_overlap_when_diagnose_then_safe_to_merge() -> Result<()> {
        let dir = repo()?;
        git(dir.path(), &["checkout", "-b", "auwsx/issue-8"])?;
        write_file(dir.path(), "feature.txt", "feature\n")?;
        commit(dir.path(), "feature")?;
        git(dir.path(), &["checkout", "main"])?;
        write_file(dir.path(), "notes.txt", "dirty main\n")?;

        let report = first_report(
            dir.path(),
            issue(8, IssueStatus::ReadyToMerge, "auwsx/issue-8"),
        )?;

        assert_eq!(report.diagnosis, ReconcileDiagnosis::SafeToMerge);
        assert_eq!(report.proposed_action, ReconcileActionKind::ApplyMerge);
        Ok(())
    }

    #[test]
    fn given_conflicting_ready_branch_when_diagnose_then_queue_agentic_reconcile() -> Result<()> {
        let dir = repo()?;
        git(dir.path(), &["checkout", "-b", "auwsx/issue-4"])?;
        write_file(dir.path(), "app.txt", "branch\n")?;
        commit(dir.path(), "branch edit")?;
        git(dir.path(), &["checkout", "main"])?;
        write_file(dir.path(), "app.txt", "main\n")?;
        commit(dir.path(), "main edit")?;

        let report = first_report(
            dir.path(),
            issue(4, IssueStatus::ReadyToMerge, "auwsx/issue-4"),
        )?;

        assert_eq!(report.diagnosis, ReconcileDiagnosis::MergeConflict);
        assert_eq!(
            report.proposed_action,
            ReconcileActionKind::QueueAgenticReconcile
        );
        Ok(())
    }

    #[test]
    fn given_agent_mark_done_for_conflict_when_validated_then_rejected() -> Result<()> {
        let mut report = ProjectReconcileReport::empty(1, true);
        report.issues.push(ReconcileIssueReport {
            issue_id: 4,
            status: IssueStatus::ReadyToMerge,
            branch: Some("auwsx/issue-4".to_string()),
            worktree_path: None,
            diagnosis: ReconcileDiagnosis::MergeConflict,
            confidence: 95,
            proposed_action: ReconcileActionKind::QueueAgenticReconcile,
            blocking_reason: None,
            manual_command: None,
        });

        assert!(validate_agent_action(&report, 4, ReconcileActionKind::MarkDone).is_err());
        Ok(())
    }

    #[test]
    fn given_final_fenced_json_proposal_when_parsed_then_accepts() -> Result<()> {
        let text = r#"Analysis before the final proposal.

```json
{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "proposal": "retry failed issue",
  "rationale": "failed issue is retryable",
  "actions": [
    {
      "action": "retry_issue",
      "issue_id": 9,
      "rationale": "transient failure"
    }
  ],
  "verification": null,
  "risk": null
}
```"#;

        let proposal = parse_agent_proposal(text)?;

        assert_eq!(proposal.actions.len(), 1);
        assert_eq!(proposal.actions[0].action, ReconcileActionKind::RetryIssue);
        Ok(())
    }

    #[test]
    fn given_embedded_json_without_final_fence_when_parsed_then_rejected() {
        let text = r#"Analysis:
{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": []
}
More analysis."#;

        assert!(parse_agent_proposal(text).is_err());
    }

    #[test]
    fn given_raw_json_proposal_when_parsed_then_rejected() {
        let text = r#"{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": []
}"#;

        assert!(parse_agent_proposal(text).is_err());
    }

    #[test]
    fn given_proposal_with_unknown_field_when_parsed_then_rejected() {
        let text = r#"```json
{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": [],
  "unexpected": true
}
```"#;

        assert!(parse_agent_proposal(text).is_err());
    }

    #[test]
    fn given_multiple_json_fences_when_proposal_parsed_then_rejected() {
        let text = r#"```json
{"note":"draft"}
```
```json
{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": []
}
```"#;

        assert!(parse_agent_proposal(text).is_err());
    }

    #[test]
    fn given_trailing_text_after_json_fence_when_proposal_parsed_then_rejected() {
        let text = r#"```json
{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": []
}
```
extra"#;

        assert!(parse_agent_proposal(text).is_err());
    }

    #[test]
    fn given_unsupported_proposal_version_when_parsed_then_rejected() {
        let text = r#"```json
{
  "schema_version": 2,
  "kind": "auwsx_reconcile_proposal",
  "actions": []
}
```"#;

        assert!(parse_agent_proposal(text).is_err());
    }

    #[test]
    fn given_wrong_proposal_kind_when_parsed_then_rejected() {
        let text = r#"```json
{
  "schema_version": 1,
  "kind": "other",
  "actions": []
}
```"#;

        assert!(parse_agent_proposal(text).is_err());
    }

    #[test]
    fn given_unknown_agent_action_when_proposal_parsed_then_rejected() {
        let text = r#"```json
{
  "schema_version": 1,
  "kind": "auwsx_reconcile_proposal",
  "actions": [
    {
      "action": "delete_branch",
      "issue_id": 9
    }
  ]
}
```"#;

        assert!(parse_agent_proposal(text).is_err());
    }

    #[test]
    fn given_cleanup_for_non_auwsx_branch_when_validated_then_rejected() {
        let mut report = ProjectReconcileReport::empty(1, true);
        report.issues.push(report_issue(
            10,
            IssueStatus::Done,
            Some("feature/manual"),
            Some("/repo/.auwsx/issue-10"),
            ReconcileDiagnosis::StaleNoDiffBranch,
            ReconcileActionKind::CleanupWorktree,
        ));

        assert!(validate_agent_action(&report, 10, ReconcileActionKind::CleanupWorktree).is_err());
    }

    #[test]
    fn given_retry_for_non_failed_issue_when_validated_then_rejected() {
        let mut report = ProjectReconcileReport::empty(1, true);
        report.issues.push(report_issue(
            11,
            IssueStatus::ReadyToMerge,
            Some("auwsx/issue-11"),
            Some("/repo/.auwsx/issue-11"),
            ReconcileDiagnosis::SafeToMerge,
            ReconcileActionKind::ApplyMerge,
        ));

        assert!(validate_agent_action(&report, 11, ReconcileActionKind::RetryIssue).is_err());
    }
}

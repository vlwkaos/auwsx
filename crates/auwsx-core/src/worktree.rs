//! Worktree port + the wsx-core adapter.
//!
//! A standalone issue gets its own git worktree at `NEW -> PLANNING`
//! and runs every later phase there; it is torn down at `DONE`. The port lets
//! the scheduler/pipeline be tested with a temp-dir fake while production drives
//! real `git worktree` via `wsx_core` (whose ops are synchronous, so the adapter
//! runs them on a blocking thread).

use crate::db::projects::Project;
use crate::Result;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use tokio::sync::Mutex;

const ISSUE_BRANCH_PREFIX: &str = "auwsx/issue-";
static GIT_WORKTREE_OPS: OnceLock<Mutex<()>> = OnceLock::new();

fn git_worktree_ops_lock() -> &'static Mutex<()> {
    GIT_WORKTREE_OPS.get_or_init(|| Mutex::new(()))
}

/// A created worktree: where it lives and the branch it tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHandle {
    pub branch: String,
    pub path: PathBuf,
}

/// Create/destroy issue worktrees. Implementations must be idempotent-friendly:
/// the pipeline only calls [`create`](Worktrees::create) when the issue has no
/// `worktree_path` yet.
#[async_trait]
pub trait Worktrees: Send + Sync {
    /// Create a worktree for `branch` off the project's default branch (running
    /// the project's copy + post-create hooks). Returns where it landed.
    async fn create(&self, project: &Project, branch: &str) -> Result<WorktreeHandle>;

    /// Remove a worktree and delete its branch (best-effort).
    async fn teardown(&self, project: &Project, handle: &WorktreeHandle) -> Result<()>;
}

/// Branch name for an issue: `auwsx/issue-{id}`. Stable + collision-free per
/// project (issue ids are unique), and greppable as auwsx-managed.
pub fn branch_for_issue(issue_id: i64) -> String {
    format!("{ISSUE_BRANCH_PREFIX}{issue_id}")
}

/// Parse the id from an auwsx-managed issue branch.
pub fn issue_id_from_branch(branch: &str) -> Option<i64> {
    let id = branch.strip_prefix(ISSUE_BRANCH_PREFIX)?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse::<i64>().ok().filter(|id| *id > 0)
}

/// A git worktree that belongs to an auwsx issue branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueWorktree {
    pub issue_id: i64,
    pub handle: WorktreeHandle,
}

/// Pick auwsx issue worktrees that are not represented by the DB anymore.
pub fn orphaned_issue_worktrees(
    worktrees: &[IssueWorktree],
    known_paths: &HashMap<i64, PathBuf>,
) -> Vec<IssueWorktree> {
    worktrees
        .iter()
        .filter(|wt| {
            known_paths
                .get(&wt.issue_id)
                .is_none_or(|path| path != &wt.handle.path)
        })
        .cloned()
        .collect()
}

fn branch_ref_exists(repo_path: &Path, branch: &str) -> Result<bool> {
    let ref_name = format!("refs/heads/{branch}");
    let status = wsx_core::git::git_cmd(repo_path)
        .args(["show-ref", "--verify", "--quiet", &ref_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("checking branch {branch}"))?;
    Ok(status.success())
}

fn archive_branch_candidate(repo_path: &Path, branch: &str) -> Result<String> {
    let issue_id =
        issue_id_from_branch(branch).ok_or_else(|| anyhow!("not an auwsx issue branch"))?;
    let base = format!("auwsx/orphaned/issue-{issue_id}");
    for suffix in 0..1000 {
        let candidate = if suffix == 0 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        if !branch_ref_exists(repo_path, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(anyhow!("could not find archive branch name for {branch}"))
}

fn prune_git_worktree_registry(repo_path: &Path) -> Result<()> {
    let status = wsx_core::git::git_cmd(repo_path)
        .args(["worktree", "prune"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| "pruning stale git worktree registry entries")?;
    if !status.success() {
        return Err(anyhow!("git worktree prune exited {status}"));
    }
    Ok(())
}

fn archive_issue_branch_if_exists(repo_path: &Path, branch: &str) -> Result<Option<String>> {
    if issue_id_from_branch(branch).is_none() {
        return Ok(None);
    }
    if !branch_ref_exists(repo_path, branch)? {
        return Ok(None);
    }
    let archive_branch = archive_branch_candidate(repo_path, branch)?;
    let status = wsx_core::git::git_cmd(repo_path)
        .args(["branch", "-m", branch, &archive_branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("archiving stale branch {branch} as {archive_branch}"))?;
    if !status.success() {
        return Err(anyhow!(
            "git branch -m {branch} {archive_branch} exited {status}"
        ));
    }
    Ok(Some(archive_branch))
}

fn prepare_issue_branch_for_create(repo_path: &Path, branch: &str) -> Result<Option<String>> {
    if issue_id_from_branch(branch).is_none() {
        return Ok(None);
    }
    // ^ DB resets can reuse issue ids while git still has auwsx/issue-N refs.
    let entries = wsx_core::git::worktree::list_worktrees(repo_path)?;
    let mut saw_stale_registry_entry = false;
    for entry in entries.iter().filter(|entry| entry.branch == branch) {
        if entry.path.exists() {
            bail!(
                "auwsx issue branch {branch} is already checked out at {}; refusing to overwrite live worktree",
                entry.path.display()
            );
        }
        saw_stale_registry_entry = true;
    }
    if saw_stale_registry_entry {
        prune_git_worktree_registry(repo_path)?;
    }
    archive_issue_branch_if_exists(repo_path, branch)
}

/// List auwsx issue worktrees recorded in git's worktree registry for `repo_path`.
pub async fn list_issue_worktrees(repo_path: &Path) -> Result<Vec<IssueWorktree>> {
    let repo_path = repo_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Vec<IssueWorktree>> {
        let entries = wsx_core::git::worktree::list_worktrees(&repo_path)?;
        Ok(entries
            .into_iter()
            .filter(|entry| !entry.is_main)
            .filter_map(|entry| {
                let issue_id = issue_id_from_branch(&entry.branch)?;
                Some(IssueWorktree {
                    issue_id,
                    handle: WorktreeHandle {
                        branch: entry.branch,
                        path: entry.path,
                    },
                })
            })
            .collect())
    })
    .await?
}

/// Remove auwsx issue worktrees that no longer match DB-owned worktree paths.
pub async fn prune_orphaned_issue_worktrees(
    repo_path: &Path,
    known_paths: &HashMap<i64, PathBuf>,
) -> Result<Vec<WorktreeHandle>> {
    let candidates = list_issue_worktrees(repo_path).await?;
    let orphans = orphaned_issue_worktrees(&candidates, known_paths);
    if orphans.is_empty() {
        return Ok(Vec::new());
    }

    let repo_path = repo_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Vec<WorktreeHandle>> {
        let mut removed = Vec::new();
        for orphan in orphans {
            remove_issue_worktree(&repo_path, &orphan.handle)?;
            removed.push(orphan.handle);
        }
        Ok(removed)
    })
    .await?
}

fn remove_issue_worktree(repo_path: &Path, handle: &WorktreeHandle) -> Result<()> {
    if issue_id_from_branch(&handle.branch).is_none() {
        bail!(
            "refusing to remove non-auwsx issue branch {}",
            handle.branch
        );
    }
    wsx_core::ops::delete_worktree(&repo_path.to_path_buf(), &handle.path, &handle.branch, &[])?;
    force_delete_issue_branch(repo_path, &handle.branch)
}

fn force_delete_issue_branch(repo_path: &Path, branch: &str) -> Result<()> {
    if issue_id_from_branch(branch).is_none() {
        bail!("refusing to force-delete non-auwsx issue branch {branch}");
    }
    let ref_name = format!("refs/heads/{branch}");
    let exists = wsx_core::git::git_cmd(repo_path)
        .args(["show-ref", "--verify", "--quiet", &ref_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("checking branch {branch}"))?;
    if !exists.success() {
        return Ok(());
    }
    let status = wsx_core::git::git_cmd(repo_path)
        .args(["branch", "-D", branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("deleting branch {branch}"))?;
    if !status.success() {
        return Err(anyhow!("git branch -D {branch} exited {status}"));
    }
    Ok(())
}

/// Production adapter over `wsx_core::ops`.
#[derive(Debug, Clone, Copy, Default)]
pub struct WsxWorktrees;

#[async_trait]
impl Worktrees for WsxWorktrees {
    async fn create(&self, project: &Project, branch: &str) -> Result<WorktreeHandle> {
        let _guard = git_worktree_ops_lock().lock().await;
        let repo_path = PathBuf::from(&project.repo_path);
        let default_branch = project.default_branch.clone();
        let branch = branch.to_string();
        let branch_for_handle = branch.clone();
        // wsx_core ops shell out to git synchronously — keep them off the runtime.
        let path = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            let archived = prepare_issue_branch_for_create(&repo_path, &branch)?;
            if let Some(archive_branch) = archived {
                tracing::warn!(
                    "archived stale auwsx issue branch {branch} as {archive_branch} before worktree create"
                );
            }
            let proj_config = wsx_core::config::project::load_project_config(&repo_path);
            let (path, _warning) =
                wsx_core::ops::create_worktree(&repo_path, &default_branch, &proj_config, &branch)?;
            Ok(path)
        })
        .await??;
        Ok(WorktreeHandle {
            branch: branch_for_handle,
            path,
        })
    }

    async fn teardown(&self, project: &Project, handle: &WorktreeHandle) -> Result<()> {
        let _guard = git_worktree_ops_lock().lock().await;
        let repo_path = PathBuf::from(&project.repo_path);
        let wt_path = handle.path.clone();
        let branch = handle.branch.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let handle = WorktreeHandle {
                branch,
                path: wt_path,
            };
            remove_issue_worktree(&repo_path, &handle)?;
            Ok(())
        })
        .await??;
        Ok(())
    }
}

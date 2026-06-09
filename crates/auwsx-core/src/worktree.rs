//! Worktree port + the wsx-core adapter.
//!
//! A standalone issue gets its own git worktree at `CONSOLIDATING -> PLANNING`
//! and runs every later phase there; it is torn down at `DONE`. The port lets
//! the scheduler/pipeline be tested with a temp-dir fake while production drives
//! real `git worktree` via `wsx_core` (whose ops are synchronous, so the adapter
//! runs them on a blocking thread).

use crate::db::projects::Project;
use crate::Result;
use async_trait::async_trait;
use std::path::PathBuf;

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
    format!("auwsx/issue-{issue_id}")
}

/// Production adapter over `wsx_core::ops`.
#[derive(Debug, Clone, Copy, Default)]
pub struct WsxWorktrees;

#[async_trait]
impl Worktrees for WsxWorktrees {
    async fn create(&self, project: &Project, branch: &str) -> Result<WorktreeHandle> {
        let repo_path = PathBuf::from(&project.repo_path);
        let default_branch = project.default_branch.clone();
        let branch = branch.to_string();
        let branch_for_handle = branch.clone();
        // wsx_core ops shell out to git synchronously — keep them off the runtime.
        let path = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
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
        let repo_path = PathBuf::from(&project.repo_path);
        let wt_path = handle.path.clone();
        let branch = handle.branch.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            // No tmux sessions tracked by auwsx for v1 (agents are direct
            // children), so pass an empty session list.
            wsx_core::ops::delete_worktree(&repo_path, &wt_path, &branch, &[])?;
            Ok(())
        })
        .await??;
        Ok(())
    }
}

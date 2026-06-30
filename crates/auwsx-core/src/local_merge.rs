//! Deterministic local merge helper for issue branches.
//!
//! The merge worker calls this through IPC instead of hand-rolling stash/merge
//! shell steps. It protects the primary worktree's dirty state by taking a
//! named stash including untracked files, merging, and restoring that stash.

use crate::Result;
use anyhow::{bail, Context};
use std::path::Path;
use std::process::{Command, Output, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMergeResult {
    pub issue_id: i64,
    pub branch: String,
    pub dirty_snapshot: Option<String>,
    pub merge_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMergeBlocked {
    pub stage: LocalMergeStage,
    pub message: String,
    pub dirty_snapshot: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMergeStage {
    Preflight,
    Snapshot,
    Merge,
    RestoreDirtyState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalMergeOutcome {
    Merged(LocalMergeResult),
    Blocked(LocalMergeBlocked),
}

pub fn merge_issue_branch(
    repo_path: &Path,
    issue_id: i64,
    branch: &str,
) -> Result<LocalMergeOutcome> {
    let repo_path = repo_path.to_path_buf();
    if issue_id <= 0 {
        bail!("issue id must be positive");
    }
    if branch.trim().is_empty() {
        bail!("issue branch is required");
    }
    if !is_worktree_clean(&repo_path)? {
        let snapshot = stash_name(issue_id);
        let outcome = snapshot_dirty_state(&repo_path, &snapshot)?;
        match outcome {
            SnapshotOutcome::CleanAfterSnapshot => {
                match merge_and_restore(&repo_path, issue_id, branch, Some(snapshot))? {
                    LocalMergeOutcome::Merged(result) => Ok(LocalMergeOutcome::Merged(result)),
                    LocalMergeOutcome::Blocked(blocked) => Ok(LocalMergeOutcome::Blocked(blocked)),
                }
            }
            SnapshotOutcome::Blocked(message) => {
                Ok(LocalMergeOutcome::Blocked(LocalMergeBlocked {
                    stage: LocalMergeStage::Snapshot,
                    message,
                    dirty_snapshot: Some(snapshot),
                }))
            }
        }
    } else {
        merge_and_restore(&repo_path, issue_id, branch, None)
    }
}

fn merge_and_restore(
    repo_path: &Path,
    issue_id: i64,
    branch: &str,
    snapshot: Option<String>,
) -> Result<LocalMergeOutcome> {
    let merge_message = format!("merge issue {issue_id}");
    let merge = git_output(
        repo_path,
        &["merge", "--no-ff", branch, "-m", &merge_message],
        LocalMergeStage::Merge,
    )?;
    if !merge.output.status.success() {
        return Ok(LocalMergeOutcome::Blocked(LocalMergeBlocked {
            stage: LocalMergeStage::Merge,
            message: merge.summary(),
            dirty_snapshot: snapshot,
        }));
    }

    if let Some(snapshot_name) = snapshot.clone() {
        let restore = git_output(
            repo_path,
            &["stash", "pop", "stash@{0}"],
            LocalMergeStage::RestoreDirtyState,
        )?;
        if !restore.output.status.success() {
            return Ok(LocalMergeOutcome::Blocked(LocalMergeBlocked {
                stage: LocalMergeStage::RestoreDirtyState,
                message: restore.summary(),
                dirty_snapshot: Some(snapshot_name),
            }));
        }
    }

    Ok(LocalMergeOutcome::Merged(LocalMergeResult {
        issue_id,
        branch: branch.to_string(),
        dirty_snapshot: snapshot,
        merge_commit: current_head(repo_path).ok(),
    }))
}

enum SnapshotOutcome {
    CleanAfterSnapshot,
    Blocked(String),
}

fn snapshot_dirty_state(repo_path: &Path, snapshot: &str) -> Result<SnapshotOutcome> {
    let status = git_output(
        repo_path,
        &["stash", "push", "--include-untracked", "-m", snapshot],
        LocalMergeStage::Snapshot,
    )?;
    if !status.output.status.success() {
        return Ok(SnapshotOutcome::Blocked(status.summary()));
    }
    if is_worktree_clean(repo_path)? {
        Ok(SnapshotOutcome::CleanAfterSnapshot)
    } else {
        Ok(SnapshotOutcome::Blocked(
            "primary worktree remained dirty after safety stash".to_string(),
        ))
    }
}

fn is_worktree_clean(repo_path: &Path) -> Result<bool> {
    let output = git(
        repo_path,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .with_context(|| format!("checking dirty state in {}", repo_path.display()))?;
    if !output.status.success() {
        bail!("{}", command_summary(LocalMergeStage::Preflight, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn current_head(repo_path: &Path) -> Result<String> {
    let output = git(repo_path, &["rev-parse", "HEAD"])
        .with_context(|| format!("reading HEAD in {}", repo_path.display()))?;
    if !output.status.success() {
        bail!("{}", command_summary(LocalMergeStage::Preflight, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn stash_name(issue_id: i64) -> String {
    format!("auwsx-pre-merge-{issue_id}-{}", unix_millis())
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

struct GitRun {
    stage: LocalMergeStage,
    args: Vec<String>,
    output: Output,
}

impl GitRun {
    fn summary(&self) -> String {
        let body = command_summary(self.stage, &self.output);
        format!("git {} failed: {body}", self.args.join(" "))
    }
}

fn git_output(repo_path: &Path, args: &[&str], stage: LocalMergeStage) -> Result<GitRun> {
    let output = git(repo_path, args)
        .with_context(|| format!("running git {} in {}", args.join(" "), repo_path.display()))?;
    Ok(GitRun {
        stage,
        args: args.iter().map(|arg| arg.to_string()).collect(),
        output,
    })
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

fn command_summary(stage: LocalMergeStage, output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format!("{stdout}\n{stderr}");
    let cleaned = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(12)
        .collect::<Vec<_>>()
        .join("; ");
    if cleaned.is_empty() {
        format!("{stage:?} exited with {}", output.status)
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn given_clean_main_when_merge_issue_branch_then_creates_merge_commit() -> Result<()> {
        let repo = repo_with_issue_branch("feature change")?;

        let outcome = merge_issue_branch(repo.path(), 7, "auwsx/issue-7")?;

        let LocalMergeOutcome::Merged(result) = outcome else {
            panic!("expected merge");
        };
        assert_eq!(result.dirty_snapshot, None);
        assert!(log_oneline(repo.path())?.contains("merge issue 7"));
        Ok(())
    }

    #[test]
    fn given_dirty_tracked_main_when_merge_issue_branch_then_restores_dirty_change() -> Result<()> {
        let repo = repo_with_issue_branch("feature change")?;
        fs::write(repo.path().join("local.txt"), "dirty tracked\n")?;

        let outcome = merge_issue_branch(repo.path(), 8, "auwsx/issue-7")?;

        assert!(matches!(outcome, LocalMergeOutcome::Merged(_)));
        assert_eq!(
            fs::read_to_string(repo.path().join("local.txt"))?,
            "dirty tracked\n"
        );
        assert!(status_porcelain(repo.path())?.contains(" M local.txt"));
        Ok(())
    }

    #[test]
    fn given_dirty_untracked_main_when_merge_issue_branch_then_restores_untracked_file(
    ) -> Result<()> {
        let repo = repo_with_issue_branch("feature change")?;
        fs::write(repo.path().join("scratch.md"), "untracked\n")?;

        let outcome = merge_issue_branch(repo.path(), 9, "auwsx/issue-7")?;

        assert!(matches!(outcome, LocalMergeOutcome::Merged(_)));
        assert_eq!(
            fs::read_to_string(repo.path().join("scratch.md"))?,
            "untracked\n"
        );
        assert!(status_porcelain(repo.path())?.contains("?? scratch.md"));
        Ok(())
    }

    #[test]
    fn given_dirty_main_overlaps_issue_change_when_restore_conflicts_then_blocks() -> Result<()> {
        let repo = repo_with_issue_branch("feature change")?;
        fs::write(repo.path().join("tracked.txt"), "dirty main\n")?;

        let outcome = merge_issue_branch(repo.path(), 10, "auwsx/issue-7")?;

        let LocalMergeOutcome::Blocked(blocked) = outcome else {
            panic!("expected restore conflict");
        };
        assert_eq!(blocked.stage, LocalMergeStage::RestoreDirtyState);
        assert!(blocked.dirty_snapshot.is_some());
        assert!(status_porcelain(repo.path())?.contains("tracked.txt"));
        Ok(())
    }

    fn repo_with_issue_branch(feature_contents: &str) -> Result<TempDir> {
        let dir = tempfile::Builder::new()
            .prefix("auwsx-local-merge-")
            .tempdir()?;
        run_git(dir.path(), &["init"])?;
        run_git(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        )?;
        run_git(dir.path(), &["config", "user.name", "auwsx test"])?;
        run_git(dir.path(), &["checkout", "-b", "main"])?;
        fs::write(dir.path().join("tracked.txt"), "base\n")?;
        fs::write(dir.path().join("local.txt"), "clean\n")?;
        run_git(dir.path(), &["add", "tracked.txt", "local.txt"])?;
        run_git(dir.path(), &["commit", "-m", "base"])?;
        run_git(dir.path(), &["checkout", "-b", "auwsx/issue-7"])?;
        fs::write(
            dir.path().join("tracked.txt"),
            format!("{feature_contents}\n"),
        )?;
        run_git(dir.path(), &["add", "tracked.txt"])?;
        run_git(dir.path(), &["commit", "-m", "issue change"])?;
        run_git(dir.path(), &["checkout", "main"])?;
        Ok(dir)
    }

    fn run_git(repo_path: &Path, args: &[&str]) -> Result<()> {
        let output = git(repo_path, args)?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                command_summary(LocalMergeStage::Preflight, &output)
            ));
        }
        Ok(())
    }

    fn status_porcelain(repo_path: &Path) -> Result<String> {
        let output = git(
            repo_path,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        if !output.status.success() {
            bail!("{}", command_summary(LocalMergeStage::Preflight, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn log_oneline(repo_path: &Path) -> Result<String> {
        let output = git(repo_path, &["log", "--oneline", "-3"])?;
        if !output.status.success() {
            bail!("{}", command_summary(LocalMergeStage::Preflight, &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

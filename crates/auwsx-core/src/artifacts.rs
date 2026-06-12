//! Paths for daemon-owned run artifacts.
//!
//! These are outside repo/worktree state. Worktrees may contain agent-created
//! `.auwsx/` files; logs and prompts recorded by the daemon live under the
//! auwsx data directory so the UI can tail them uniformly.

use crate::Result;
use anyhow::Context;
use directories::ProjectDirs;
use std::path::Path;
use std::path::PathBuf;

pub fn issue_run_paths(issue_id: i64, spawned_at: i64) -> Result<(PathBuf, PathBuf)> {
    let base = data_dir().join("runs").join(format!("issue-{issue_id}"));
    std::fs::create_dir_all(&base)
        .with_context(|| format!("creating issue run dir {}", base.display()))?;
    Ok((
        base.join(format!("run-{spawned_at}.log")),
        base.join(format!("run-{spawned_at}.prompt.txt")),
    ))
}

pub fn main_job_log_path(project_id: i64, main_job_id: i64, started_at: i64) -> Result<PathBuf> {
    let base = data_dir()
        .join("main-jobs")
        .join(format!("project-{project_id}"));
    std::fs::create_dir_all(&base)
        .with_context(|| format!("creating main job log dir {}", base.display()))?;
    Ok(base.join(format!("job-{main_job_id}-{started_at}.log")))
}

pub fn data_dir() -> PathBuf {
    if let Ok(env) = std::env::var("AUWSX_DATA_DIR") {
        return PathBuf::from(env);
    }
    if let Some(dirs) = ProjectDirs::from("", "", "auwsx") {
        return dirs.data_dir().to_path_buf();
    }
    std::env::temp_dir().join("auwsx")
}

pub async fn tail_file(path: PathBuf, max_bytes: usize) -> Result<String> {
    tokio::task::spawn_blocking(move || tail_file_blocking(&path, max_bytes))
        .await
        .context("tail log worker panicked")?
}

fn tail_file_blocking(path: &Path, max_bytes: usize) -> Result<String> {
    let max_bytes = max_bytes.clamp(1, 256 * 1024);
    let meta = std::fs::metadata(path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("{} is not a file", path.display());
    }
    let len = meta.len();
    let start = len.saturating_sub(max_bytes as u64);
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(max_bytes as u64).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    #[tokio::test]
    async fn given_data_dir_env_when_issue_paths_then_stable_run_files_under_issue_dir(
    ) -> anyhow::Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        std::env::set_var("AUWSX_DATA_DIR", tmp.path());

        let (log, prompt) = issue_run_paths(42, 1234)?;

        assert_eq!(log, tmp.path().join("runs/issue-42/run-1234.log"));
        assert_eq!(prompt, tmp.path().join("runs/issue-42/run-1234.prompt.txt"));
        assert!(log.parent().expect("parent").is_dir());

        std::env::remove_var("AUWSX_DATA_DIR");
        Ok(())
    }

    #[tokio::test]
    async fn given_large_log_when_tail_file_then_returns_suffix_only() -> anyhow::Result<()> {
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("run.log");
        let mut file = std::fs::File::create(&path)?;
        write!(file, "0123456789abcdef")?;

        let tail = tail_file(path, 6).await?;

        assert_eq!(tail, "abcdef");
        Ok(())
    }
}

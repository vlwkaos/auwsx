use anyhow::{bail, ensure, Context, Result};
use auwsx_core::agent::{codex, ExitKind};
use auwsx_core::db::{agent_runs, issues, scheduler_runs, Db};
use auwsx_core::state::IssueStatus;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Output, Stdio};
use std::time::Duration;

const BACKLOG_COUNT: usize = 7;

#[tokio::test]
#[ignore = "spawns auwsx daemon, git worktrees, and real local merges"]
async fn deterministic_agent_drives_many_backlogs_to_done_with_serial_local_merges() -> Result<()> {
    let harness = Harness::new("lm-")?;
    harness.init_repo()?;
    let agent = harness.write_script("real-agent.sh", DETERMINISTIC_AGENT)?;
    let _daemon = harness.start_daemon()?;
    harness.wait_for_daemon()?;

    let project_id = harness.add_project(&agent)?;
    for idx in 1..=BACKLOG_COUNT {
        harness.auwsx_ok(&[
            "backlog",
            "add",
            &project_id.to_string(),
            &format!("real backlog {idx}"),
        ])?;
    }

    let db = Db::open_at(&harness.db_path).await?;
    let mut last_statuses = Vec::new();
    for _ in 0..260 {
        harness.auwsx_ok(&["scheduler", "run", &project_id.to_string()])?;
        let project_issues = issues::list_by_project(db.pool(), project_id).await?;
        last_statuses = project_issues
            .iter()
            .map(|issue| (issue.id, issue.status))
            .collect();

        if let Some((issue_id, status)) = blocked_or_failed(&last_statuses) {
            bail!(
                "issue {issue_id} stopped at {status:?}\n{}",
                failure_context(&harness, &db, project_id, issue_id).await?
            );
        }
        if project_issues.len() == BACKLOG_COUNT
            && project_issues
                .iter()
                .all(|issue| issue.status == IssueStatus::Done)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let project_issues = issues::list_by_project(db.pool(), project_id).await?;
    ensure!(
        project_issues.len() == BACKLOG_COUNT,
        "expected {BACKLOG_COUNT} issues, got {}",
        project_issues.len()
    );
    ensure!(
        project_issues
            .iter()
            .all(|issue| issue.status == IssueStatus::Done),
        "issues did not all finish: {:?}",
        last_statuses
    );
    ensure!(
        project_issues
            .iter()
            .all(|issue| issue.branch.is_none() && issue.worktree_path.is_none()),
        "done issues must have cleaned branch/worktree handles: {:?}",
        project_issues
    );

    let status = harness.git(&["status", "--short"])?;
    ensure!(status.trim().is_empty(), "main repo is dirty:\n{status}");
    let files = harness.git(&["ls-files", "work"])?;
    let committed: Vec<&str> = files.lines().collect();
    ensure!(
        committed.len() == BACKLOG_COUNT,
        "expected {BACKLOG_COUNT} committed work files, got {:?}",
        committed
    );

    assert_scheduler_filled_available_slots(&db, project_id).await?;
    assert_local_merge_runs_did_not_overlap(&db, &project_issues).await?;
    assert_each_issue_run_has_phase_report(&db, &project_issues).await?;

    Ok(())
}

#[tokio::test]
#[ignore = "requires real local Codex or AUWSX_E2E_AGENT_CMD credentials"]
async fn configured_llm_agent_can_drive_one_issue_to_terminal() -> Result<()> {
    let agent_cmd = std::env::var("AUWSX_E2E_AGENT_CMD")
        .ok()
        .filter(|value| !value.trim().is_empty());

    let harness = Harness::new("llm-")?;
    harness.init_repo()?;
    let _daemon = harness.start_daemon()?;
    harness.wait_for_daemon()?;

    let project_id = if let Some(agent_cmd) = agent_cmd {
        harness.add_project_with_cmds(&agent_cmd, &agent_cmd, &agent_cmd, &agent_cmd)?
    } else {
        harness.add_project_with_arsenal(codex::NAME)?
    };
    harness.auwsx_ok(&[
        "backlog",
        "add",
        &project_id.to_string(),
        "create a small tracked file named work/llm-e2e.txt and complete the issue",
    ])?;

    let db = Db::open_at(&harness.db_path).await?;
    for _ in 0..180 {
        harness.auwsx_ok(&["scheduler", "run", &project_id.to_string()])?;
        let project_issues = issues::list_by_project(db.pool(), project_id).await?;
        if let Some(issue) = project_issues.first() {
            if issue.status == IssueStatus::Done {
                return Ok(());
            }
            if matches!(
                issue.status,
                IssueStatus::Failed
                    | IssueStatus::PlanBlocked
                    | IssueStatus::ReviewBlocked
                    | IssueStatus::ConflictBlocked
            ) {
                bail!("LLM pipeline stopped at attention state: {:?}", issue);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    bail!("LLM pipeline did not reach DONE before timeout");
}

async fn assert_scheduler_filled_available_slots(db: &Db, project_id: i64) -> Result<()> {
    let runs = scheduler_runs::recent_by_project(db.pool(), project_id, 200).await?;
    let mut saw_initial_fill = false;
    let mut saw_full_running = false;

    for run in runs {
        let Some(picked) = run.picked else {
            continue;
        };
        let picked: scheduler_runs::SchedulerRunPicked = serde_json::from_str(&picked)?;
        let spawn_count = picked
            .decisions
            .iter()
            .filter(|decision| {
                matches!(decision, scheduler_runs::SchedulerRunDecision::Spawn { .. })
            })
            .count();
        if picked.triaged_issue_ids.len() == BACKLOG_COUNT && spawn_count == 3 {
            saw_initial_fill = true;
        }
        if picked.running_issues == 3 && picked.decisions.is_empty() {
            saw_full_running = true;
        }
    }

    ensure!(
        saw_initial_fill,
        "scheduler never filled three initial slots"
    );
    ensure!(
        saw_full_running,
        "scheduler never recorded a full running set without over-scheduling"
    );
    Ok(())
}

async fn assert_local_merge_runs_did_not_overlap(
    db: &Db,
    project_issues: &[issues::Issue],
) -> Result<()> {
    let mut merge_runs = Vec::new();
    for issue in project_issues {
        for run in agent_runs::list_by_issue(db.pool(), issue.id).await? {
            if run.phase == "MERGING" {
                ensure!(
                    run.exit_kind == Some(ExitKind::Exited),
                    "merge run did not exit cleanly: {:?}",
                    run
                );
                merge_runs.push(run);
            }
        }
    }
    merge_runs.sort_by_key(|run| run.spawned_at);
    ensure!(
        merge_runs.len() == BACKLOG_COUNT,
        "expected one merge run per issue, got {}",
        merge_runs.len()
    );

    for pair in merge_runs.windows(2) {
        let left = &pair[0];
        let right = &pair[1];
        let left_exit = left
            .exited_at
            .with_context(|| format!("merge run {} was not closed", left.id))?;
        ensure!(
            right.spawned_at >= left_exit,
            "local merge runs overlapped: left={left:?} right={right:?}"
        );
    }
    Ok(())
}

async fn assert_each_issue_run_has_phase_report(
    db: &Db,
    project_issues: &[issues::Issue],
) -> Result<()> {
    for issue in project_issues {
        for run in agent_runs::list_by_issue(db.pool(), issue.id).await? {
            ensure!(
                run.phase_report
                    .as_deref()
                    .is_some_and(|report| report.contains("deterministic e2e")),
                "run missing phase report: {:?}",
                run
            );
        }
    }
    Ok(())
}

async fn failure_context(
    harness: &Harness,
    db: &Db,
    project_id: i64,
    issue_id: i64,
) -> Result<String> {
    let mut lines = Vec::new();
    lines.push(format!("root={}", harness.root.path().display()));
    let daemon_log = harness.root.path().join("daemon.log");
    if daemon_log.exists() {
        let text = fs::read_to_string(&daemon_log)
            .unwrap_or_else(|err| format!("cannot read {}: {err}", daemon_log.display()));
        lines.push(format!(
            "daemon log {}:\n{}",
            daemon_log.display(),
            tail_lines(&text, 120)
        ));
    }
    lines.push(format!(
        "issues:\n{}",
        harness.auwsx_ok(&["issue", "ls", &project_id.to_string()])?
    ));
    lines.push(format!(
        "git worktrees:\n{}",
        harness.git(&["worktree", "list", "--porcelain"])?
    ));
    lines.push(format!(
        "git branches:\n{}",
        harness.git(&["branch", "--list", "auwsx/issue-*"])?
    ));
    for run in agent_runs::list_by_issue(db.pool(), issue_id).await? {
        lines.push(format!(
            "run #{} role={:?} phase={} before={:?} after={:?} exit={:?}/{:?}",
            run.id,
            run.role,
            run.phase,
            run.status_before,
            run.status_after,
            run.exit_kind,
            run.exit_code
        ));
        if let Some(path) = run.log_path {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|err| format!("cannot read {path}: {err}"));
            lines.push(format!("log {path}:\n{}", tail_lines(&text, 80)));
        }
    }
    Ok(lines.join("\n"))
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn blocked_or_failed(statuses: &[(i64, IssueStatus)]) -> Option<(i64, IssueStatus)> {
    for (id, status) in statuses {
        if matches!(
            status,
            IssueStatus::Failed
                | IssueStatus::PlanBlocked
                | IssueStatus::ReviewBlocked
                | IssueStatus::ConflictBlocked
        ) {
            return Some((*id, *status));
        }
    }
    None
}

struct Harness {
    root: tempfile::TempDir,
    repo_path: PathBuf,
    db_path: PathBuf,
    env: BTreeMap<String, String>,
}

impl Harness {
    fn new(prefix: &str) -> Result<Self> {
        let tmp_root = workspace_root().join(".tmp");
        fs::create_dir_all(&tmp_root).context("creating .tmp")?;
        let root = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&tmp_root)
            .context("creating repo-local temp dir")?;
        let repo_path = root.path().join("repo");
        let data_dir = root.path().join("data");
        let db_path = data_dir.join("state.db");
        let socket_path = root.path().join("auwsx.sock");
        fs::create_dir_all(&data_dir).context("creating data dir")?;

        let mut env = BTreeMap::new();
        env.insert("AUWSX_DB_PATH".to_string(), db_path.display().to_string());
        env.insert("AUWSX_DATA_DIR".to_string(), data_dir.display().to_string());
        env.insert("AUWSX_SOCK".to_string(), socket_path.display().to_string());
        env.insert("AUWSX_TICK_SECS".to_string(), "60".to_string());

        Ok(Self {
            root,
            repo_path,
            db_path,
            env,
        })
    }

    fn init_repo(&self) -> Result<()> {
        fs::create_dir_all(&self.repo_path).context("creating git repo dir")?;
        self.git(&["init", "-b", "main"])?;
        self.git(&["config", "user.name", "auwsx e2e"])?;
        self.git(&["config", "user.email", "auwsx-e2e@example.invalid"])?;
        fs::write(self.repo_path.join("README.md"), "auwsx e2e\n").context("writing README")?;
        fs::write(
            self.repo_path.join("AGENTS.md"),
            "## auwsx Knowledge Collections\n<!-- auwsx:knowledge-collections -->\n",
        )
        .context("writing AGENTS.md")?;
        self.git(&["add", "README.md", "AGENTS.md"])?;
        self.git(&["commit", "-m", "initial"])?;
        Ok(())
    }

    fn write_script(&self, name: &str, body: &str) -> Result<PathBuf> {
        let path = self.root.path().join(name);
        fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        make_executable(&path)?;
        Ok(path)
    }

    fn start_daemon(&self) -> Result<TestDaemon> {
        let log_path = self.root.path().join("daemon.log");
        let log = fs::File::create(&log_path).context("creating daemon log")?;
        let err = log.try_clone().context("cloning daemon log")?;
        let child = self
            .auwsx_command(&["daemon"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .spawn()
            .context("starting auwsx daemon")?;
        Ok(TestDaemon { child })
    }

    fn wait_for_daemon(&self) -> Result<()> {
        let mut last = None;
        for _ in 0..80 {
            match self.auwsx_output(&["ping"]) {
                Ok(output) if output.status.success() => return Ok(()),
                Ok(output) => last = Some(output_text(&output)),
                Err(err) => last = Some(err.to_string()),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("daemon did not become ready: {}", last.unwrap_or_default())
    }

    fn add_project(&self, agent_path: &Path) -> Result<i64> {
        let cmd = format!("bash {}", agent_path.display());
        self.add_project_with_cmds(&cmd, &cmd, &cmd, &cmd)
    }

    fn add_project_with_cmds(
        &self,
        main_cmd: &str,
        plan_cmd: &str,
        work_cmd: &str,
        review_cmd: &str,
    ) -> Result<i64> {
        let out = self.auwsx_ok(&[
            "project",
            "add",
            "real",
            &self.repo_path.display().to_string(),
            "--main",
            main_cmd,
            "--plan",
            plan_cmd,
            "--work",
            work_cmd,
            "--review",
            review_cmd,
            "--completion-policy",
            "auto",
            "--plan-gate-timeout",
            "0",
        ])?;
        out.trim()
            .parse::<i64>()
            .with_context(|| format!("parsing project id from {out:?}"))
    }

    fn add_project_with_arsenal(&self, preset: &str) -> Result<i64> {
        let out = self.auwsx_ok(&[
            "project",
            "add",
            "real",
            &self.repo_path.display().to_string(),
            "--arsenal",
            preset,
            "--completion-policy",
            "auto",
            "--plan-gate-timeout",
            "0",
        ])?;
        out.trim()
            .parse::<i64>()
            .with_context(|| format!("parsing project id from {out:?}"))
    }

    fn auwsx_ok(&self, args: &[&str]) -> Result<String> {
        let output = self.auwsx_output(args)?;
        ensure!(
            output.status.success(),
            "auwsx {:?} failed:\n{}",
            args,
            output_text(&output)
        );
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn auwsx_output(&self, args: &[&str]) -> Result<Output> {
        self.auwsx_command(args)
            .output()
            .with_context(|| format!("running auwsx {args:?}"))
    }

    fn auwsx_command(&self, args: &[&str]) -> ProcessCommand {
        let mut cmd = ProcessCommand::new(env!("CARGO_BIN_EXE_auwsx"));
        cmd.args(args).envs(&self.env);
        cmd
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let output = ProcessCommand::new("git")
            .args(args)
            .current_dir(&self.repo_path)
            .output()
            .with_context(|| format!("running git {args:?}"))?;
        ensure!(
            output.status.success(),
            "git {:?} failed:\n{}",
            args,
            output_text(&output)
        );
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("auwsx-tui lives under crates/")
        .to_path_buf()
}

struct TestDaemon {
    child: Child,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

const DETERMINISTIC_AGENT: &str = r#"#!/usr/bin/env bash
set -euo pipefail

issue_field() {
  "$AUWSX_BIN" issue get "$AUWSX_ISSUE_ID" | sed -n "s/^$1: //p" | head -n 1
}

status="$(issue_field status)"
title="$(issue_field title)"
repo="$(git worktree list --porcelain | sed -n 's/^worktree //p' | head -n 1)"

git config user.name "auwsx real test"
git config user.email "auwsx-real-test@example.invalid"
mkdir -p .auwsx
agent_log=".auwsx/real-agent-${AUWSX_ISSUE_ID}.log"
if [ "$status" != "MERGING" ]; then
  printf 'issue=%s\nstatus=%s\nrole=%s\ntitle=%s\n' "$AUWSX_ISSUE_ID" "$status" "$AUWSX_AGENT_ROLE" "$title" >> "$agent_log"
fi
printf 'deterministic e2e phase %s for issue %s\nrole=%s\n' "$status" "$AUWSX_ISSUE_ID" "$AUWSX_AGENT_ROLE" > .auwsx/phase-report.md

case "$status:$AUWSX_AGENT_ROLE" in
  PLANNING:plan)
    "$AUWSX_BIN" subtask add "$AUWSX_ISSUE_ID" 1 "write real file for issue $AUWSX_ISSUE_ID"
    sleep 0.2
    "$AUWSX_BIN" issue status "$AUWSX_ISSUE_ID" PLAN_READY
    ;;
  WORKING:work)
    mkdir -p work
    printf 'issue %s\n%s\n' "$AUWSX_ISSUE_ID" "$title" > "work/issue-${AUWSX_ISSUE_ID}.txt"
    git add "work/issue-${AUWSX_ISSUE_ID}.txt" "$agent_log"
    git commit -m "issue ${AUWSX_ISSUE_ID}: implement backlog"
    sleep 0.2
    "$AUWSX_BIN" issue status "$AUWSX_ISSUE_ID" REVIEWING
    ;;
  REVIEWING:review)
    test -f "work/issue-${AUWSX_ISSUE_ID}.txt"
    sleep 0.2
    "$AUWSX_BIN" issue status "$AUWSX_ISSUE_ID" AUDITING
    ;;
  AUDITING:work)
    test -f "work/issue-${AUWSX_ISSUE_ID}.txt"
    if [ -n "$(git status --short)" ]; then
      git add "$agent_log"
      git commit -m "issue ${AUWSX_ISSUE_ID}: record audit"
    fi
    sleep 0.2
    "$AUWSX_BIN" issue status "$AUWSX_ISSUE_ID" READY_TO_MERGE
    ;;
  MERGING:work)
    branch="$(git branch --show-current)"
    test -n "$repo"
    test -n "$branch"
    git rebase main
    git -C "$repo" merge --no-ff "$branch" -m "merge issue ${AUWSX_ISSUE_ID}"
    test -f "$repo/work/issue-${AUWSX_ISSUE_ID}.txt"
    sleep 0.2
    "$AUWSX_BIN" issue status "$AUWSX_ISSUE_ID" DONE
    ;;
  RESOLVING_CONFLICT:work)
    "$AUWSX_BIN" issue status "$AUWSX_ISSUE_ID" CONFLICT_BLOCKED
    ;;
  *)
    printf 'unexpected phase status=%s role=%s\n' "$status" "$AUWSX_AGENT_ROLE"
    exit 64
    ;;
esac
"#;

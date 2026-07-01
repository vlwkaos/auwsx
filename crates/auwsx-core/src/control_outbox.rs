//! Worktree-local agent control channel.
//!
//! Codex's workspace sandbox does not reliably expose Unix socket files, even
//! when the socket is created inside the worktree. Issue agents therefore write
//! control commands to a normal JSONL outbox. The daemon owns replay after the
//! agent exits, so DB mutation still stays centralized.

use crate::db::agent_runs::Role;
use crate::db::findings::Finding;
use crate::db::issues::Issue;
use crate::db::subtasks::Subtask;
use crate::events::Event;
use crate::ipc::{self, Command, Response};
use crate::state::{is_legal_transition, IssueStatus};
use crate::steering::Steering;
use crate::{Error, Result};
use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::Path;
use tokio::sync::broadcast;

pub const OUTBOX_ENV: &str = "AUWSX_CONTROL_OUTBOX";
pub const SNAPSHOT_ENV: &str = "AUWSX_CONTROL_SNAPSHOT";
const MAX_OUTBOX_BYTES: u64 = 256 * 1024;
const MAX_OUTBOX_LINE_BYTES: usize = 16 * 1024;
const MAX_OUTBOX_COMMANDS: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub issue: Issue,
    pub subtasks: Vec<Subtask>,
    pub findings: Vec<Finding>,
    pub steering: Vec<Steering>,
}

impl ControlSnapshot {
    pub fn allowed_subtask_ids(&self) -> HashSet<i64> {
        self.subtasks.iter().map(|s| s.id).collect()
    }

    pub fn allowed_finding_ids(&self) -> HashSet<i64> {
        self.findings.iter().map(|f| f.id).collect()
    }
}

pub fn write_snapshot(path: &Path, snapshot: &ControlSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating control snapshot dir {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(snapshot).context("encoding control snapshot")?;
    std::fs::write(path, bytes)
        .with_context(|| format!("writing control snapshot {}", path.display()))?;
    Ok(())
}

/// If the process is running as an issue agent control shim, handle `cmd`
/// locally and return a printable response. Otherwise return `Ok(None)` so the
/// normal IPC path can run.
pub fn handle_local_command(cmd: &Command) -> Result<Option<Response>> {
    let Some(outbox) = std::env::var_os(OUTBOX_ENV) else {
        return Ok(None);
    };
    let issue_id = std::env::var("AUWSX_ISSUE_ID")
        .context("AUWSX_ISSUE_ID is required with AUWSX_CONTROL_OUTBOX")?
        .parse::<i64>()
        .context("AUWSX_ISSUE_ID must be an integer")?;

    let snapshot = read_snapshot_from_env()?;
    let normalized;
    let cmd = if let Command::CompleteSubtask { subtask_id } = cmd {
        if snapshot.allowed_subtask_ids().contains(subtask_id) {
            cmd
        } else if let Some(subtask) = snapshot.subtasks.iter().find(|t| t.ord == *subtask_id) {
            normalized = Command::CompleteSubtask {
                subtask_id: subtask.id,
            };
            &normalized
        } else {
            cmd
        }
    } else {
        cmd
    };

    let response = match cmd {
        Command::Ping => Response::Ok,
        Command::GetIssue { issue_id: id } if *id == issue_id => {
            Response::Issue(Some(snapshot.issue.clone()))
        }
        Command::ListSubtasks { issue_id: id } if *id == issue_id => {
            Response::Subtasks(snapshot.subtasks.clone())
        }
        Command::ListFindings {
            issue_id: id,
            open_only,
        } if *id == issue_id => {
            let findings = if *open_only {
                snapshot
                    .findings
                    .clone()
                    .into_iter()
                    .filter(|f| f.status == crate::db::findings::FindingStatus::Open)
                    .collect()
            } else {
                snapshot.findings.clone()
            };
            Response::Findings(findings)
        }
        Command::ListSteering { issue_id: id, .. } if *id == issue_id => {
            Response::Steering(snapshot.steering.clone())
        }
        Command::AddSubtask { issue_id: id, .. }
        | Command::SetIssueStatus { issue_id: id, .. }
        | Command::ApplyIssueMerge { issue_id: id }
        | Command::AddFinding { issue_id: id, .. }
            if *id == issue_id =>
        {
            append_command(Path::new(&outbox), cmd)?;
            Response::Ok
        }
        command
            if {
                is_recordable_for_issue(
                    command,
                    issue_id,
                    &snapshot.allowed_subtask_ids(),
                    &snapshot.allowed_finding_ids(),
                )
            } =>
        {
            append_command(Path::new(&outbox), command)?;
            Response::Ok
        }
        _ => Response::Err {
            message: "command is not available through issue-local control outbox".to_string(),
        },
    };

    Ok(Some(response))
}

fn read_snapshot_from_env() -> Result<ControlSnapshot> {
    let path = std::env::var_os(SNAPSHOT_ENV)
        .ok_or_else(|| anyhow!("AUWSX_CONTROL_SNAPSHOT is required with AUWSX_CONTROL_OUTBOX"))?;
    let path = Path::new(&path);
    let data = std::fs::read(path)
        .with_context(|| format!("reading control snapshot {}", path.display()))?;
    serde_json::from_slice(&data)
        .with_context(|| format!("decoding control snapshot {}", path.display()))
}

fn append_command(path: &Path, cmd: &Command) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating control outbox dir {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening control outbox {}", path.display()))?;
    let mut line = serde_json::to_vec(cmd).context("encoding control command")?;
    line.push(b'\n');

    // ^ Issue agents can fire multiple control CLI commands at once. Lock and
    // write one full record so JSON objects do not concatenate on one line.
    file.lock()
        .with_context(|| format!("locking control outbox {}", path.display()))?;
    let write_result = file
        .write_all(&line)
        .and_then(|_| file.flush())
        .with_context(|| format!("writing control outbox {}", path.display()));
    let unlock_result = file
        .unlock()
        .with_context(|| format!("unlocking control outbox {}", path.display()));
    write_result?;
    unlock_result
}

pub async fn replay(
    db: &crate::db::Db,
    events: &broadcast::Sender<Event>,
    issue_id: i64,
    snapshot: &ControlSnapshot,
    outbox_path: &Path,
    now: i64,
) -> Result<Vec<Response>> {
    let allowed_subtasks = snapshot.allowed_subtask_ids();
    let allowed_findings = snapshot.allowed_finding_ids();
    let commands = decode_commands_from_file(outbox_path)?;
    validate_replay_commands(
        &commands,
        issue_id,
        snapshot,
        &allowed_subtasks,
        &allowed_findings,
    )?;

    let mut responses = Vec::new();
    for (_, cmd) in commands {
        let resp = ipc::dispatch(db, events, now, cmd).await;
        responses.push(resp);
    }
    Ok(responses)
}

fn validate_replay_commands(
    commands: &[(usize, Command)],
    issue_id: i64,
    snapshot: &ControlSnapshot,
    allowed_subtasks: &HashSet<i64>,
    allowed_findings: &HashSet<i64>,
) -> Result<()> {
    let mut status_commands = 0;
    for (line_no, cmd) in commands {
        if !is_recordable_for_issue(cmd, issue_id, allowed_subtasks, allowed_findings) {
            bail!(
                "control outbox line {} is not valid for issue {}",
                line_no,
                issue_id
            );
        }
        if let Command::SetIssueStatus { status, force, .. } = cmd {
            status_commands += 1;
            if *force {
                bail!(
                    "control outbox line {} cannot force issue status transitions",
                    line_no
                );
            }
            if !is_legal_transition(snapshot.issue.status, *status) {
                bail!(
                    "control outbox line {} has illegal transition {} -> {}",
                    line_no,
                    snapshot.issue.status.as_str(),
                    status.as_str()
                );
            }
        } else if matches!(cmd, Command::ApplyIssueMerge { .. }) {
            status_commands += 1;
            if snapshot.issue.status != IssueStatus::Merging {
                bail!(
                    "control outbox line {} can apply merge only from MERGING, got {}",
                    line_no,
                    snapshot.issue.status.as_str()
                );
            }
        }
    }
    if commands.is_empty() {
        return Ok(());
    }
    match status_commands {
        1 => Ok(()),
        0 => bail!("control outbox is missing a final issue status command"),
        n => bail!("control outbox has {n} issue status commands; expected exactly one"),
    }
}

fn decode_commands_from_file(path: &Path) -> Result<Vec<(usize, Command)>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::from(e))
                .with_context(|| format!("opening control outbox {}", path.display()))
        }
    };
    let len = file
        .metadata()
        .with_context(|| format!("reading control outbox metadata {}", path.display()))?
        .len();
    if len > MAX_OUTBOX_BYTES {
        bail!(
            "control outbox {} is too large: {} bytes > {}",
            path.display(),
            len,
            MAX_OUTBOX_BYTES
        );
    }

    let mut commands = Vec::new();
    for (idx, line) in std::io::BufReader::new(file).split(b'\n').enumerate() {
        let line_no = idx + 1;
        let bytes = line.with_context(|| {
            format!(
                "reading control outbox line {line_no} from {}",
                path.display()
            )
        })?;
        if bytes.len() > MAX_OUTBOX_LINE_BYTES {
            bail!(
                "control outbox line {} is too long: {} bytes > {}",
                line_no,
                bytes.len(),
                MAX_OUTBOX_LINE_BYTES
            );
        }
        let raw = String::from_utf8(bytes)
            .with_context(|| format!("decoding control outbox line {line_no} as UTF-8"))?;
        commands.extend(decode_command_line(line_no, &raw)?);
        if commands.len() > MAX_OUTBOX_COMMANDS {
            bail!(
                "control outbox has too many commands: {} > {}",
                commands.len(),
                MAX_OUTBOX_COMMANDS
            );
        }
    }
    Ok(commands)
}

#[cfg(test)]
fn decode_commands(data: &str) -> Result<Vec<(usize, Command)>> {
    let mut commands = Vec::new();
    for (idx, raw) in data.lines().enumerate() {
        let line_no = idx + 1;
        commands.extend(decode_command_line(line_no, raw)?);
    }
    Ok(commands)
}

fn decode_command_line(line_no: usize, raw: &str) -> Result<Vec<(usize, Command)>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }

    let mut commands = Vec::new();
    let stream = serde_json::Deserializer::from_str(raw).into_iter::<Command>();
    for decoded in stream {
        let cmd = decoded.with_context(|| {
            format!(
                "decoding control outbox line {} near {}",
                line_no,
                preview(raw)
            )
        })?;
        commands.push((line_no, cmd));
    }
    Ok(commands)
}

fn preview(raw: &str) -> String {
    const MAX: usize = 120;
    if raw.chars().count() <= MAX {
        raw.to_string()
    } else {
        format!("{}...", raw.chars().take(MAX).collect::<String>())
    }
}

fn is_recordable_for_issue(
    cmd: &Command,
    issue_id: i64,
    allowed_subtasks: &HashSet<i64>,
    allowed_findings: &HashSet<i64>,
) -> bool {
    match cmd {
        Command::AddSubtask { issue_id: id, .. }
        | Command::SetIssueStatus { issue_id: id, .. }
        | Command::ApplyIssueMerge { issue_id: id }
        | Command::AddFinding { issue_id: id, .. } => *id == issue_id,
        Command::CompleteSubtask { subtask_id } => allowed_subtasks.contains(subtask_id),
        Command::AcceptFinding { finding_id, .. }
        | Command::RejectFinding { finding_id, .. }
        | Command::DismissFinding { finding_id } => allowed_findings.contains(finding_id),
        _ => false,
    }
}

#[allow(dead_code)]
pub fn role_can_use_outbox(role: Role) -> bool {
    matches!(role, Role::Plan | Role::Work | Role::Review)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::IssueStatus;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn issue() -> Issue {
        Issue {
            id: 7,
            project_id: 1,
            title: "issue".to_string(),
            description: None,
            agent_summary: None,
            progress_report: None,
            result_report: None,
            status: IssueStatus::Working,
            branch: None,
            worktree_path: None,
            review_round: 0,
            conflict_attempts: 0,
            wait_until: None,
            has_pending_steering: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn subtask() -> Subtask {
        Subtask {
            id: 11,
            issue_id: 7,
            ord: 1,
            text: "do it".to_string(),
            done: false,
            created_at: 1,
            done_at: None,
        }
    }

    fn finding() -> Finding {
        Finding {
            id: 17,
            issue_id: 7,
            review_round: 0,
            severity: crate::db::findings::Severity::Minor,
            lens: None,
            title: "finding".to_string(),
            detail: None,
            file_ref: None,
            status: crate::db::findings::FindingStatus::Open,
            adjudication: None,
            created_at: 1,
            resolved_at: None,
        }
    }

    fn snapshot() -> ControlSnapshot {
        ControlSnapshot {
            issue: issue(),
            subtasks: vec![subtask()],
            findings: vec![finding()],
            steering: Vec::new(),
        }
    }

    fn with_env<T>(snapshot: &Path, outbox: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        std::env::set_var(SNAPSHOT_ENV, snapshot);
        std::env::set_var(OUTBOX_ENV, outbox);
        std::env::set_var("AUWSX_ISSUE_ID", "7");
        let result = f();
        std::env::remove_var(SNAPSHOT_ENV);
        std::env::remove_var(OUTBOX_ENV);
        std::env::remove_var("AUWSX_ISSUE_ID");
        result
    }

    #[test]
    fn given_local_control_env_when_list_subtasks_then_reads_snapshot() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let snapshot_path = tmp.path().join("snapshot.json");
        let outbox_path = tmp.path().join("outbox.jsonl");
        write_snapshot(&snapshot_path, &snapshot())?;

        let response = with_env(&snapshot_path, &outbox_path, || {
            handle_local_command(&Command::ListSubtasks { issue_id: 7 })
        })?
        .expect("local response");

        match response {
            Response::Subtasks(items) => assert_eq!(items[0].id, 11),
            other => panic!("unexpected response: {other:?}"),
        }
        assert!(!outbox_path.exists());
        Ok(())
    }

    #[test]
    fn given_local_control_env_when_subtask_done_uses_ord_then_records_subtask_id(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let snapshot_path = tmp.path().join("snapshot.json");
        let outbox_path = tmp.path().join("outbox.jsonl");
        write_snapshot(&snapshot_path, &snapshot())?;

        let response = with_env(&snapshot_path, &outbox_path, || {
            handle_local_command(&Command::CompleteSubtask { subtask_id: 1 })
        })?
        .expect("local response");

        assert!(matches!(response, Response::Ok));
        let raw = std::fs::read_to_string(outbox_path)?;
        let command: Command = serde_json::from_str(raw.trim())?;
        assert_eq!(command, Command::CompleteSubtask { subtask_id: 11 });
        Ok(())
    }

    #[test]
    fn given_local_control_env_when_status_set_then_records_command() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let snapshot_path = tmp.path().join("snapshot.json");
        let outbox_path = tmp.path().join("outbox.jsonl");
        write_snapshot(&snapshot_path, &snapshot())?;

        let response = with_env(&snapshot_path, &outbox_path, || {
            handle_local_command(&Command::SetIssueStatus {
                issue_id: 7,
                status: IssueStatus::Reviewing,
                force: false,
            })
        })?
        .expect("local response");

        assert!(matches!(response, Response::Ok));
        let raw = std::fs::read_to_string(outbox_path)?;
        let command: Command = serde_json::from_str(raw.trim())?;
        assert_eq!(
            command,
            Command::SetIssueStatus {
                issue_id: 7,
                status: IssueStatus::Reviewing,
                force: false,
            }
        );
        Ok(())
    }

    #[test]
    fn given_oversized_control_outbox_when_decoded_from_file_then_err() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let outbox_path = tmp.path().join("outbox.jsonl");
        std::fs::write(&outbox_path, " ".repeat((MAX_OUTBOX_BYTES + 1) as usize))?;

        let err = decode_commands_from_file(&outbox_path)
            .expect_err("oversized worker outbox must be rejected");

        assert!(
            err.to_string().contains("too large"),
            "unexpected error: {err:#}"
        );
        Ok(())
    }

    #[test]
    fn given_local_control_env_when_apply_merge_then_records_command() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let snapshot_path = tmp.path().join("snapshot.json");
        let outbox_path = tmp.path().join("outbox.jsonl");
        let mut snapshot = snapshot();
        snapshot.issue.status = IssueStatus::Merging;
        write_snapshot(&snapshot_path, &snapshot)?;

        let response = with_env(&snapshot_path, &outbox_path, || {
            handle_local_command(&Command::ApplyIssueMerge { issue_id: 7 })
        })?
        .expect("local response");

        assert!(matches!(response, Response::Ok));
        let raw = std::fs::read_to_string(outbox_path)?;
        let command: Command = serde_json::from_str(raw.trim())?;
        assert_eq!(command, Command::ApplyIssueMerge { issue_id: 7 });
        Ok(())
    }

    #[test]
    fn given_local_control_env_when_finding_accepted_then_records_command() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let snapshot_path = tmp.path().join("snapshot.json");
        let outbox_path = tmp.path().join("outbox.jsonl");
        write_snapshot(&snapshot_path, &snapshot())?;

        let response = with_env(&snapshot_path, &outbox_path, || {
            handle_local_command(&Command::AcceptFinding {
                finding_id: 17,
                rationale: "will fix".to_string(),
            })
        })?
        .expect("local response");

        assert!(matches!(response, Response::Ok));
        let raw = std::fs::read_to_string(outbox_path)?;
        let command: Command = serde_json::from_str(raw.trim())?;
        assert_eq!(
            command,
            Command::AcceptFinding {
                finding_id: 17,
                rationale: "will fix".to_string(),
            }
        );
        Ok(())
    }

    #[test]
    fn given_local_control_env_when_other_finding_accepted_then_rejects_command(
    ) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let snapshot_path = tmp.path().join("snapshot.json");
        let outbox_path = tmp.path().join("outbox.jsonl");
        write_snapshot(&snapshot_path, &snapshot())?;

        let response = with_env(&snapshot_path, &outbox_path, || {
            handle_local_command(&Command::AcceptFinding {
                finding_id: 99,
                rationale: "will fix".to_string(),
            })
        })?
        .expect("local response");

        assert!(matches!(response, Response::Err { .. }));
        assert!(!outbox_path.exists());
        Ok(())
    }

    #[test]
    fn given_adjacent_json_objects_when_decode_commands_then_recovers_both() -> anyhow::Result<()> {
        let raw = r#"{"cmd":"complete_subtask","subtask_id":2}{"cmd":"complete_subtask","subtask_id":1}

{"cmd":"set_issue_status","issue_id":7,"status":"REVIEWING","force":false}
"#;

        let commands = decode_commands(raw)?;

        assert_eq!(
            commands,
            vec![
                (1, Command::CompleteSubtask { subtask_id: 2 }),
                (1, Command::CompleteSubtask { subtask_id: 1 }),
                (
                    3,
                    Command::SetIssueStatus {
                        issue_id: 7,
                        status: IssueStatus::Reviewing,
                        force: false,
                    }
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn given_invalid_control_outbox_when_decode_commands_then_reports_line_context() {
        let err = decode_commands("{\"cmd\":\"complete_subtask\" nope").expect_err("decode error");

        let message = format!("{err:#}");
        assert!(message.contains("decoding control outbox line 1 near"));
        assert!(message.contains("{\"cmd\":\"complete_subtask\" nope"));
    }

    #[test]
    fn given_outbox_without_status_when_validated_then_err() -> anyhow::Result<()> {
        let commands = vec![(
            1,
            Command::AddSubtask {
                issue_id: 7,
                ord: 1,
                text: "plan".to_string(),
            },
        )];
        let snapshot = snapshot();

        let err = validate_replay_commands(
            &commands,
            7,
            &snapshot,
            &snapshot.allowed_subtask_ids(),
            &snapshot.allowed_finding_ids(),
        )
        .expect_err("nonempty outbox must include final status");

        assert!(err.to_string().contains("missing a final issue status"));
        Ok(())
    }

    #[test]
    fn given_outbox_with_forced_status_when_validated_then_err() -> anyhow::Result<()> {
        let commands = vec![(
            1,
            Command::SetIssueStatus {
                issue_id: 7,
                status: IssueStatus::Reviewing,
                force: true,
            },
        )];
        let snapshot = snapshot();

        let err = validate_replay_commands(
            &commands,
            7,
            &snapshot,
            &snapshot.allowed_subtask_ids(),
            &snapshot.allowed_finding_ids(),
        )
        .expect_err("issue-local outbox must not force transitions");

        assert!(err.to_string().contains("cannot force"));
        Ok(())
    }
}

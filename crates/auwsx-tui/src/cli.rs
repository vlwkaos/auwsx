//! `auwsx` command-line surface: a thin IPC client plus the daemon entry point.
//!
//! [`parse`] is pure (args in, [`CliAction`] out) so the whole arg grammar is
//! unit-testable without a socket; [`run_daemon`] / [`run_request`] are the
//! runtime glue. Every non-daemon subcommand becomes one [`Command`] sent to the
//! running daemon over the Unix socket — the daemon owns all DB writes.

use anyhow::{bail, Context, Result};
use auwsx_core::backlog::{Approval, Source};
use auwsx_core::db::findings::Severity;
use auwsx_core::db::projects::CompletionPolicy;
use auwsx_core::events;
use auwsx_core::ipc::{self, Command, Response};
use auwsx_core::state::IssueStatus;
use auwsx_core::steering::SteeringSource;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Notify;

/// What the parsed argv asks `auwsx` to do.
#[derive(Debug, Clone, PartialEq)]
pub enum CliAction {
    /// Run the daemon (IPC server) in the foreground.
    Daemon,
    /// Send one command to the running daemon and print the reply.
    Request(Command),
    /// Print usage.
    Help,
    /// No recognized subcommand — caller falls through to the TUI.
    Tui,
}

const USAGE: &str = "\
auwsx — autonomous workspace orchestrator

USAGE:
  auwsx                         launch the TUI (default; not yet implemented)
  auwsx daemon                  run the daemon (IPC server) in the foreground
  auwsx daemon stop             ask the running daemon to shut down
  auwsx ping                    check the daemon is up

  auwsx project add <name> <repo_path> --branch <b> \\
        --main <cmd> --plan <cmd> --work <cmd> [--review <cmd>] \\
        [--completion-policy manual|soft|auto] \\
        [--plan-gate-timeout <min>] [--completion-timeout <min>]
  auwsx project ls

  auwsx backlog add <project_id> <text...> [--source human|agent|routine|inbox]
  auwsx backlog ls <project_id> [--approval pending|approved|dismissed]
  auwsx backlog approve <item_id>
  auwsx backlog dismiss <item_id>
  auwsx triage <project_id>

  auwsx issue add <project_id> <title...> [--desc <text>]
  auwsx issue ls <project_id> [--status <STATUS>]
  auwsx issue get <issue_id>
  auwsx issue status <issue_id> <STATUS> [--force]

  auwsx subtask add <issue_id> <ord> <text...>
  auwsx subtask ls <issue_id>
  auwsx subtask done <subtask_id>

  auwsx finding add <issue_id> <round> <severity> <title...> \\
        [--lens <l>] [--detail <d>] [--file <ref>]
  auwsx finding ls <issue_id> [--open]
  auwsx finding accept <finding_id> <rationale...>
  auwsx finding reject <finding_id> <rationale...>
  auwsx finding dismiss <finding_id>

  auwsx steering add <issue_id> <source> <note...>     (source: human|consolidation)
  auwsx steering ls <issue_id>
  auwsx steering consume <issue_id>
";

/// Parse argv (without the program name) into a [`CliAction`]. Pure: no IO.
pub fn parse(args: &[String]) -> Result<CliAction> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Ok(CliAction::Tui);
    };
    match sub {
        "help" | "--help" | "-h" => Ok(CliAction::Help),
        "ping" => Ok(CliAction::Request(Command::Ping)),
        "daemon" => match args.get(1).map(String::as_str) {
            None => Ok(CliAction::Daemon),
            Some("stop") => Ok(CliAction::Request(Command::Shutdown)),
            Some(other) => bail!("unknown `daemon` subcommand: {other}"),
        },
        "project" => parse_project(&args[1..]),
        "backlog" => parse_backlog(&args[1..]),
        "triage" => {
            let p = Parsed::new(&args[1..]);
            Ok(CliAction::Request(Command::Triage {
                project_id: p.int(0, "project_id")?,
            }))
        }
        "issue" => parse_issue(&args[1..]),
        "subtask" => parse_subtask(&args[1..]),
        "finding" => parse_finding(&args[1..]),
        "steering" => parse_steering(&args[1..]),
        // Unknown leading token: let the caller decide (TUI fallback).
        _ => Ok(CliAction::Tui),
    }
}

fn parse_project(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "project")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "add" => Command::AddProject {
            name: p.pos(0, "name")?,
            repo_path: p.pos(1, "repo_path")?,
            default_branch: p.flag("branch").unwrap_or_else(|| "main".to_string()),
            main_agent_cmd: p.req_flag("main")?,
            plan_agent_cmd: p.req_flag("plan")?,
            work_agent_cmd: p.req_flag("work")?,
            review_agent_cmd: p.flag("review"),
            completion_policy: match p.flag("completion-policy") {
                Some(s) => Some(
                    CompletionPolicy::from_str(&s)
                        .with_context(|| format!("invalid --completion-policy {s:?}"))?,
                ),
                None => None,
            },
            plan_gate_timeout_min: p.opt_int("plan-gate-timeout")?,
            completion_soft_timeout_min: p.opt_int("completion-timeout")?,
        },
        "ls" | "list" => Command::ListProjects,
        other => bail!("unknown `project` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_backlog(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "backlog")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "add" => Command::AddBacklog {
            project_id: p.int(0, "project_id")?,
            text: p.rest_from(1, "text")?,
            source: match p.flag("source") {
                Some(s) => Source::from_str(&s)
                    .with_context(|| format!("invalid --source {s:?}"))?,
                None => Source::Human,
            },
        },
        "ls" | "list" => Command::ListBacklog {
            project_id: p.int(0, "project_id")?,
            approval: match p.flag("approval") {
                Some(a) => Some(
                    Approval::from_str(&a)
                        .with_context(|| format!("invalid --approval {a:?}"))?,
                ),
                None => None,
            },
        },
        "approve" => Command::ApproveBacklog {
            item_id: p.int(0, "item_id")?,
        },
        "dismiss" => Command::DismissBacklog {
            item_id: p.int(0, "item_id")?,
        },
        other => bail!("unknown `backlog` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_issue(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "issue")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "add" => Command::AddIssue {
            project_id: p.int(0, "project_id")?,
            title: p.rest_from(1, "title")?,
            description: p.flag("desc"),
        },
        "ls" | "list" => Command::ListIssues {
            project_id: p.int(0, "project_id")?,
            status: match p.flag("status") {
                Some(s) => Some(
                    IssueStatus::from_str(&s)
                        .with_context(|| format!("invalid --status {s:?}"))?,
                ),
                None => None,
            },
        },
        "get" => Command::GetIssue {
            issue_id: p.int(0, "issue_id")?,
        },
        "status" => Command::SetIssueStatus {
            issue_id: p.int(0, "issue_id")?,
            status: {
                let s = p.pos(1, "STATUS")?;
                IssueStatus::from_str(&s).with_context(|| format!("invalid status {s:?}"))?
            },
            force: p.has("force"),
        },
        other => bail!("unknown `issue` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_subtask(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "subtask")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "add" => Command::AddSubtask {
            issue_id: p.int(0, "issue_id")?,
            ord: p.int(1, "ord")?,
            text: p.rest_from(2, "text")?,
        },
        "ls" | "list" => Command::ListSubtasks {
            issue_id: p.int(0, "issue_id")?,
        },
        "done" => Command::CompleteSubtask {
            subtask_id: p.int(0, "subtask_id")?,
        },
        other => bail!("unknown `subtask` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_finding(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "finding")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "add" => Command::AddFinding {
            issue_id: p.int(0, "issue_id")?,
            review_round: p.int(1, "round")?,
            severity: {
                let s = p.pos(2, "severity")?;
                Severity::from_str(&s).with_context(|| format!("invalid severity {s:?}"))?
            },
            title: p.rest_from(3, "title")?,
            lens: p.flag("lens"),
            detail: p.flag("detail"),
            file_ref: p.flag("file"),
        },
        "ls" | "list" => Command::ListFindings {
            issue_id: p.int(0, "issue_id")?,
            open_only: p.has("open"),
        },
        "accept" => Command::AcceptFinding {
            finding_id: p.int(0, "finding_id")?,
            rationale: p.rest_from(1, "rationale")?,
        },
        "reject" => Command::RejectFinding {
            finding_id: p.int(0, "finding_id")?,
            rationale: p.rest_from(1, "rationale")?,
        },
        "dismiss" => Command::DismissFinding {
            finding_id: p.int(0, "finding_id")?,
        },
        other => bail!("unknown `finding` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_steering(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "steering")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "add" => Command::AddSteering {
            issue_id: p.int(0, "issue_id")?,
            source: {
                let s = p.pos(1, "source")?;
                SteeringSource::from_str(&s).with_context(|| format!("invalid source {s:?}"))?
            },
            note: p.rest_from(2, "note")?,
        },
        "ls" | "list" => Command::ListSteering {
            issue_id: p.int(0, "issue_id")?,
            pending_only: true,
        },
        "consume" => Command::ConsumeSteering {
            issue_id: p.int(0, "issue_id")?,
        },
        other => bail!("unknown `steering` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn split_verb<'a>(args: &'a [String], group: &str) -> Result<(&'a str, &'a [String])> {
    match args.split_first() {
        Some((verb, rest)) => Ok((verb.as_str(), rest)),
        None => bail!("`{group}` needs a subcommand (try `auwsx help`)"),
    }
}

/// Split args into positionals and `--key value` / `--flag` options. A `--key`
/// followed by a non-`--` token takes it as the value; otherwise it's a boolean
/// flag (value present in `bools`). Order of positionals is preserved.
struct Parsed {
    positionals: Vec<String>,
    flags: HashMap<String, String>,
    bools: Vec<String>,
}

impl Parsed {
    fn new(args: &[String]) -> Self {
        let mut positionals = Vec::new();
        let mut flags = HashMap::new();
        let mut bools = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let tok = &args[i];
            if let Some(key) = tok.strip_prefix("--") {
                // `--key=value`
                if let Some((k, v)) = key.split_once('=') {
                    flags.insert(k.to_string(), v.to_string());
                    i += 1;
                    continue;
                }
                // `--key value` when a value follows and isn't another flag
                match args.get(i + 1) {
                    Some(next) if !next.starts_with("--") => {
                        flags.insert(key.to_string(), next.clone());
                        i += 2;
                    }
                    _ => {
                        bools.push(key.to_string());
                        i += 1;
                    }
                }
            } else {
                positionals.push(tok.clone());
                i += 1;
            }
        }
        Parsed {
            positionals,
            flags,
            bools,
        }
    }

    fn pos(&self, idx: usize, name: &str) -> Result<String> {
        self.positionals
            .get(idx)
            .cloned()
            .with_context(|| format!("missing required argument <{name}>"))
    }

    fn int(&self, idx: usize, name: &str) -> Result<i64> {
        let raw = self.pos(idx, name)?;
        raw.parse::<i64>()
            .with_context(|| format!("<{name}> must be an integer, got {raw:?}"))
    }

    /// Parse an optional `--key <int>` flag. Absent ⇒ `None`; present-but-not-an
    /// integer ⇒ `Err` (so `--plan-gate-timeout abc` is rejected, not silently
    /// dropped).
    fn opt_int(&self, key: &str) -> Result<Option<i64>> {
        match self.flag(key) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<i64>()
                .map(Some)
                .with_context(|| format!("--{key} must be an integer, got {raw:?}")),
        }
    }

    /// Join positionals from `idx` to the end into one space-separated string.
    fn rest_from(&self, idx: usize, name: &str) -> Result<String> {
        if idx >= self.positionals.len() {
            bail!("missing required argument <{name}>");
        }
        Ok(self.positionals[idx..].join(" "))
    }

    fn flag(&self, key: &str) -> Option<String> {
        self.flags.get(key).cloned()
    }

    fn req_flag(&self, key: &str) -> Result<String> {
        self.flag(key)
            .with_context(|| format!("missing required option --{key}"))
    }

    fn has(&self, key: &str) -> bool {
        self.bools.iter().any(|b| b == key)
    }
}

// ---------------------------------------------------------------------------
// Runtime glue
// ---------------------------------------------------------------------------

/// Run the daemon in the foreground until SIGINT or a `Shutdown` command. Runs
/// the IPC server and the autonomous scheduler concurrently over one DB + event
/// bus; the agents the scheduler spawns call back through this same socket.
pub async fn run_daemon() -> Result<()> {
    use auwsx_core::agent::subprocess_executor;
    use auwsx_core::clock;
    use auwsx_core::db::Db;
    use auwsx_core::scheduler::Scheduler;
    use auwsx_core::worktree::WsxWorktrees;
    use std::time::Duration;

    let db = Db::open().await.context("opening database")?;
    let bus = events::channel();
    let socket = ipc::default_socket_path();
    let shutdown = Arc::new(Notify::new());

    let sd = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            sd.notify_one();
        }
    });

    // Scheduler tick cadence: how often issue status is re-read and dispatched.
    // Short so the loop reacts promptly when an agent advances an issue; the
    // running-set prevents re-spawning a still-live agent.
    let tick_secs = std::env::var("AUWSX_TICK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10)
        .max(1);
    let scheduler = Scheduler::new(
        db.clone(),
        clock::system(),
        subprocess_executor(),
        Arc::new(WsxWorktrees),
        bus.clone(),
        socket.clone(),
        Duration::from_secs(tick_secs),
    );
    let sched_stop = Arc::new(Notify::new());
    let sched_stop_run = sched_stop.clone();
    let sched_task = tokio::spawn(async move { scheduler.run(sched_stop_run).await });

    println!("auwsx daemon listening on {}", socket.display());
    println!("scheduler ticking every {tick_secs}s");
    let serve_result = ipc::serve(db, bus, &socket, shutdown).await;

    // IPC server stopped (SIGINT or `daemon stop`) — drain the scheduler too.
    sched_stop.notify_one();
    let _ = sched_task.await;

    serve_result.context("ipc server")?;
    println!("auwsx daemon stopped");
    Ok(())
}

/// Send one command to the daemon and print the reply. Exits the process with
/// status 1 (after printing to stderr) on a `Response::Err`.
pub async fn run_request(cmd: Command) -> Result<()> {
    let socket = ipc::default_socket_path();
    let resp = ipc::request(&socket, &cmd).await.with_context(|| {
        format!(
            "talking to daemon at {} (is `auwsx daemon` running?)",
            socket.display()
        )
    })?;
    if print_response(resp) {
        std::process::exit(1);
    }
    Ok(())
}

/// Print a response in a compact human form. Returns true if it was an error
/// (so the caller can set a non-zero exit code).
fn print_response(resp: Response) -> bool {
    match resp {
        Response::Ok => println!("ok"),
        Response::Id(id) => println!("{id}"),
        Response::Err { message } => {
            eprintln!("error: {message}");
            return true;
        }
        Response::Triaged { created_issue_ids } => {
            if created_issue_ids.is_empty() {
                println!("triage: nothing to promote");
            } else {
                println!("triage: created issues {created_issue_ids:?}");
            }
        }
        Response::Projects(ps) => {
            for p in ps {
                println!("{}\t{}\t{}\t{}", p.id, p.name, p.default_branch, p.repo_path);
            }
        }
        Response::Project(Some(p)) => {
            println!("id:     {}", p.id);
            println!("name:   {}", p.name);
            println!("repo:   {}", p.repo_path);
            println!("branch: {}", p.default_branch);
            println!("agents: main={} plan={} work={}", p.main_agent_cmd, p.plan_agent_cmd, p.work_agent_cmd);
            println!(
                "policy: completion={} plan_gate={}min completion_soft={}min concurrency={}",
                p.completion_policy.as_str(),
                p.plan_gate_timeout_min,
                p.completion_soft_timeout_min,
                p.max_concurrency
            );
        }
        Response::Project(None) => println!("not found"),
        Response::Backlog(items) => {
            for i in items {
                println!(
                    "{}\t{}\t{}\t{}",
                    i.id,
                    i.approval.as_str(),
                    i.source.as_str(),
                    i.text
                );
            }
        }
        Response::Issues(issues) => {
            for i in issues {
                println!("{}\t{}\t{}", i.id, i.status.as_str(), i.title);
            }
        }
        Response::Issue(Some(i)) => {
            println!("id:     {}", i.id);
            println!("status: {}", i.status.as_str());
            println!("title:  {}", i.title);
            if let Some(d) = i.description {
                println!("desc:   {d}");
            }
            if let Some(b) = i.branch {
                println!("branch: {b}");
            }
            println!(
                "rounds: review={} conflict={} pending_steering={}",
                i.review_round, i.conflict_attempts, i.has_pending_steering
            );
        }
        Response::Issue(None) => println!("not found"),
        Response::Subtasks(ss) => {
            for s in ss {
                let mark = if s.done { 'x' } else { ' ' };
                println!("{}\t[{}]\t{}\t{}", s.id, mark, s.ord, s.text);
            }
        }
        Response::Findings(fs) => {
            for f in fs {
                println!(
                    "{}\t{}\t{}\t{}",
                    f.id,
                    f.severity.as_str(),
                    f.status.as_str(),
                    f.title
                );
            }
        }
        Response::Steering(ss) => {
            for s in ss {
                println!("{}\t{}\t{}", s.id, s.source.as_str(), s.note);
            }
        }
        Response::Event(ev) => println!("event: {ev:?}"),
    }
    false
}

/// Print top-level usage.
pub fn print_usage() {
    print!("{USAGE}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    // --- top-level dispatch -------------------------------------------------

    #[test]
    fn given_empty_args_when_parsed_then_tui() {
        assert_eq!(parse(&argv(&[])).unwrap(), CliAction::Tui);
    }

    #[test]
    fn given_unknown_leading_token_when_parsed_then_tui_not_err() {
        assert_eq!(parse(&argv(&["wat"])).unwrap(), CliAction::Tui);
    }

    #[test]
    fn given_help_word_when_parsed_then_help() {
        assert_eq!(parse(&argv(&["help"])).unwrap(), CliAction::Help);
    }

    #[test]
    fn given_long_help_flag_when_parsed_then_help() {
        assert_eq!(parse(&argv(&["--help"])).unwrap(), CliAction::Help);
    }

    #[test]
    fn given_short_help_flag_when_parsed_then_help() {
        assert_eq!(parse(&argv(&["-h"])).unwrap(), CliAction::Help);
    }

    #[test]
    fn given_ping_when_parsed_then_request_ping() {
        assert_eq!(
            parse(&argv(&["ping"])).unwrap(),
            CliAction::Request(Command::Ping)
        );
    }

    // --- daemon -------------------------------------------------------------

    #[test]
    fn given_daemon_when_parsed_then_daemon() {
        assert_eq!(parse(&argv(&["daemon"])).unwrap(), CliAction::Daemon);
    }

    #[test]
    fn given_daemon_stop_when_parsed_then_request_shutdown() {
        assert_eq!(
            parse(&argv(&["daemon", "stop"])).unwrap(),
            CliAction::Request(Command::Shutdown)
        );
    }

    #[test]
    fn given_daemon_bogus_subcommand_when_parsed_then_err() {
        assert!(parse(&argv(&["daemon", "bogus"])).is_err());
    }

    // --- project add --------------------------------------------------------

    #[test]
    fn given_project_add_with_required_flags_when_parsed_then_addproject_defaults() {
        assert_eq!(
            parse(&argv(&[
                "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc"
            ]))
            .unwrap(),
            CliAction::Request(Command::AddProject {
                name: "demo".to_string(),
                repo_path: "/repo".to_string(),
                default_branch: "main".to_string(),
                main_agent_cmd: "mc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: None,
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
            })
        );
    }

    #[test]
    fn given_project_add_with_branch_and_review_when_parsed_then_fields_reflect() {
        assert_eq!(
            parse(&argv(&[
                "project", "add", "demo", "/repo", "--branch", "dev", "--main", "mc", "--plan",
                "pc", "--work", "wc", "--review", "rc"
            ]))
            .unwrap(),
            CliAction::Request(Command::AddProject {
                name: "demo".to_string(),
                repo_path: "/repo".to_string(),
                default_branch: "dev".to_string(),
                main_agent_cmd: "mc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: Some("rc".to_string()),
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
            })
        );
    }

    #[test]
    fn given_project_add_missing_required_flags_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "add", "demo", "/repo"])).is_err());
    }

    // --- project ls / list --------------------------------------------------

    #[test]
    fn given_project_ls_when_parsed_then_listprojects() {
        assert_eq!(
            parse(&argv(&["project", "ls"])).unwrap(),
            CliAction::Request(Command::ListProjects)
        );
    }

    #[test]
    fn given_project_list_alias_when_parsed_then_listprojects() {
        assert_eq!(
            parse(&argv(&["project", "list"])).unwrap(),
            CliAction::Request(Command::ListProjects)
        );
    }

    #[test]
    fn given_project_without_verb_when_parsed_then_err() {
        assert!(parse(&argv(&["project"])).is_err());
    }

    #[test]
    fn given_project_bogus_verb_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "bogus"])).is_err());
    }

    // --- backlog add --------------------------------------------------------

    #[test]
    fn given_backlog_add_multiword_when_parsed_then_addbacklog_joined_human() {
        assert_eq!(
            parse(&argv(&["backlog", "add", "1", "add", "dark", "mode"])).unwrap(),
            CliAction::Request(Command::AddBacklog {
                project_id: 1,
                text: "add dark mode".to_string(),
                source: Source::Human,
            })
        );
    }

    #[test]
    fn given_backlog_add_with_source_flag_when_parsed_then_source_agent() {
        assert_eq!(
            parse(&argv(&["backlog", "add", "1", "x", "--source", "agent"])).unwrap(),
            CliAction::Request(Command::AddBacklog {
                project_id: 1,
                text: "x".to_string(),
                source: Source::Agent,
            })
        );
    }

    #[test]
    fn given_backlog_add_with_source_equals_form_when_parsed_then_source_agent() {
        assert_eq!(
            parse(&argv(&["backlog", "add", "1", "x", "--source=agent"])).unwrap(),
            CliAction::Request(Command::AddBacklog {
                project_id: 1,
                text: "x".to_string(),
                source: Source::Agent,
            })
        );
    }

    #[test]
    fn given_backlog_add_bogus_source_when_parsed_then_err() {
        assert!(parse(&argv(&["backlog", "add", "1", "x", "--source", "bogus"])).is_err());
    }

    #[test]
    fn given_backlog_add_no_text_when_parsed_then_err() {
        assert!(parse(&argv(&["backlog", "add", "1"])).is_err());
    }

    // --- backlog ls ---------------------------------------------------------

    #[test]
    fn given_backlog_ls_when_parsed_then_listbacklog_no_approval() {
        assert_eq!(
            parse(&argv(&["backlog", "ls", "1"])).unwrap(),
            CliAction::Request(Command::ListBacklog {
                project_id: 1,
                approval: None,
            })
        );
    }

    #[test]
    fn given_backlog_ls_with_approval_when_parsed_then_some_approved() {
        assert_eq!(
            parse(&argv(&["backlog", "ls", "1", "--approval", "approved"])).unwrap(),
            CliAction::Request(Command::ListBacklog {
                project_id: 1,
                approval: Some(Approval::Approved),
            })
        );
    }

    #[test]
    fn given_backlog_ls_bogus_approval_when_parsed_then_err() {
        assert!(parse(&argv(&["backlog", "ls", "1", "--approval", "bogus"])).is_err());
    }

    #[test]
    fn given_backlog_ls_non_integer_project_id_when_parsed_then_err() {
        assert!(parse(&argv(&["backlog", "ls", "notanint"])).is_err());
    }

    // --- triage -------------------------------------------------------------

    #[test]
    fn given_triage_when_parsed_then_triage() {
        assert_eq!(
            parse(&argv(&["triage", "1"])).unwrap(),
            CliAction::Request(Command::Triage { project_id: 1 })
        );
    }

    // --- issue --------------------------------------------------------------

    #[test]
    fn given_issue_add_multiword_with_desc_when_parsed_then_addissue() {
        assert_eq!(
            parse(&argv(&["issue", "add", "2", "my", "title", "--desc", "d"])).unwrap(),
            CliAction::Request(Command::AddIssue {
                project_id: 2,
                title: "my title".to_string(),
                description: Some("d".to_string()),
            })
        );
    }

    #[test]
    fn given_issue_ls_with_status_when_parsed_then_some_planning() {
        assert_eq!(
            parse(&argv(&["issue", "ls", "1", "--status", "PLANNING"])).unwrap(),
            CliAction::Request(Command::ListIssues {
                project_id: 1,
                status: Some(IssueStatus::Planning),
            })
        );
    }

    #[test]
    fn given_issue_ls_bogus_status_when_parsed_then_err() {
        assert!(parse(&argv(&["issue", "ls", "1", "--status", "bogus"])).is_err());
    }

    #[test]
    fn given_issue_ls_without_status_when_parsed_then_status_none() {
        assert_eq!(
            parse(&argv(&["issue", "ls", "1"])).unwrap(),
            CliAction::Request(Command::ListIssues {
                project_id: 1,
                status: None,
            })
        );
    }

    #[test]
    fn given_issue_get_when_parsed_then_getissue() {
        assert_eq!(
            parse(&argv(&["issue", "get", "5"])).unwrap(),
            CliAction::Request(Command::GetIssue { issue_id: 5 })
        );
    }

    #[test]
    fn given_issue_status_when_parsed_then_setissuestatus_force_false() {
        assert_eq!(
            parse(&argv(&["issue", "status", "1", "PLANNING"])).unwrap(),
            CliAction::Request(Command::SetIssueStatus {
                issue_id: 1,
                status: IssueStatus::Planning,
                force: false,
            })
        );
    }

    #[test]
    fn given_issue_status_with_force_when_parsed_then_force_true() {
        assert_eq!(
            parse(&argv(&["issue", "status", "1", "PLANNING", "--force"])).unwrap(),
            CliAction::Request(Command::SetIssueStatus {
                issue_id: 1,
                status: IssueStatus::Planning,
                force: true,
            })
        );
    }

    #[test]
    fn given_issue_status_invalid_status_when_parsed_then_err() {
        assert!(parse(&argv(&["issue", "status", "1", "bogus"])).is_err());
    }

    // --- subtask ------------------------------------------------------------

    #[test]
    fn given_subtask_add_multiword_when_parsed_then_addsubtask() {
        assert_eq!(
            parse(&argv(&["subtask", "add", "1", "0", "do", "the", "thing"])).unwrap(),
            CliAction::Request(Command::AddSubtask {
                issue_id: 1,
                ord: 0,
                text: "do the thing".to_string(),
            })
        );
    }

    #[test]
    fn given_subtask_done_when_parsed_then_completesubtask() {
        assert_eq!(
            parse(&argv(&["subtask", "done", "3"])).unwrap(),
            CliAction::Request(Command::CompleteSubtask { subtask_id: 3 })
        );
    }

    #[test]
    fn given_subtask_ls_when_parsed_then_listsubtasks() {
        assert_eq!(
            parse(&argv(&["subtask", "ls", "1"])).unwrap(),
            CliAction::Request(Command::ListSubtasks { issue_id: 1 })
        );
    }

    // --- finding ------------------------------------------------------------

    #[test]
    fn given_finding_add_with_lens_and_file_when_parsed_then_addfinding() {
        assert_eq!(
            parse(&argv(&[
                "finding", "add", "1", "0", "blocker", "null", "deref", "--lens", "correctness",
                "--file", "src/x.rs"
            ]))
            .unwrap(),
            CliAction::Request(Command::AddFinding {
                issue_id: 1,
                review_round: 0,
                severity: Severity::Blocker,
                title: "null deref".to_string(),
                lens: Some("correctness".to_string()),
                detail: None,
                file_ref: Some("src/x.rs".to_string()),
            })
        );
    }

    #[test]
    fn given_finding_add_invalid_severity_when_parsed_then_err() {
        assert!(parse(&argv(&["finding", "add", "1", "0", "bogus", "title"])).is_err());
    }

    #[test]
    fn given_finding_ls_with_open_when_parsed_then_open_only_true() {
        assert_eq!(
            parse(&argv(&["finding", "ls", "1", "--open"])).unwrap(),
            CliAction::Request(Command::ListFindings {
                issue_id: 1,
                open_only: true,
            })
        );
    }

    #[test]
    fn given_finding_ls_without_open_when_parsed_then_open_only_false() {
        assert_eq!(
            parse(&argv(&["finding", "ls", "1"])).unwrap(),
            CliAction::Request(Command::ListFindings {
                issue_id: 1,
                open_only: false,
            })
        );
    }

    #[test]
    fn given_finding_accept_multiword_when_parsed_then_acceptfinding() {
        assert_eq!(
            parse(&argv(&["finding", "accept", "2", "looks", "right"])).unwrap(),
            CliAction::Request(Command::AcceptFinding {
                finding_id: 2,
                rationale: "looks right".to_string(),
            })
        );
    }

    #[test]
    fn given_finding_reject_multiword_when_parsed_then_rejectfinding() {
        assert_eq!(
            parse(&argv(&["finding", "reject", "2", "looks", "wrong"])).unwrap(),
            CliAction::Request(Command::RejectFinding {
                finding_id: 2,
                rationale: "looks wrong".to_string(),
            })
        );
    }

    #[test]
    fn given_finding_dismiss_when_parsed_then_dismissfinding() {
        assert_eq!(
            parse(&argv(&["finding", "dismiss", "2"])).unwrap(),
            CliAction::Request(Command::DismissFinding { finding_id: 2 })
        );
    }

    // --- steering -----------------------------------------------------------

    #[test]
    fn given_steering_add_multiword_when_parsed_then_addsteering_human() {
        assert_eq!(
            parse(&argv(&["steering", "add", "1", "human", "please", "retry"])).unwrap(),
            CliAction::Request(Command::AddSteering {
                issue_id: 1,
                source: SteeringSource::Human,
                note: "please retry".to_string(),
            })
        );
    }

    #[test]
    fn given_steering_add_invalid_source_when_parsed_then_err() {
        assert!(parse(&argv(&["steering", "add", "1", "bogus", "note"])).is_err());
    }

    #[test]
    fn given_steering_consume_when_parsed_then_consumesteering() {
        assert_eq!(
            parse(&argv(&["steering", "consume", "1"])).unwrap(),
            CliAction::Request(Command::ConsumeSteering { issue_id: 1 })
        );
    }

    #[test]
    fn given_steering_ls_when_parsed_then_liststeering_pending_only() {
        assert_eq!(
            parse(&argv(&["steering", "ls", "1"])).unwrap(),
            CliAction::Request(Command::ListSteering {
                issue_id: 1,
                pending_only: true,
            })
        );
    }

    // --- project add: completion-policy / gate-timeout flags ----------------

    #[test]
    fn given_baseline_when_parsed_then_addproject_with_all_optional_fields_none() {
        assert_eq!(
            parse(&argv(&[
                "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc"
            ]))
            .unwrap(),
            CliAction::Request(Command::AddProject {
                name: "demo".to_string(),
                repo_path: "/repo".to_string(),
                default_branch: "main".to_string(),
                main_agent_cmd: "mc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: None,
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
            })
        );
    }

    #[test]
    fn given_completion_policy_auto_when_parsed_then_policy_some_auto() {
        let CliAction::Request(Command::AddProject { completion_policy, .. }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-policy", "auto",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_policy, Some(CompletionPolicy::Auto));
    }

    #[test]
    fn given_completion_policy_soft_when_parsed_then_policy_some_soft() {
        let CliAction::Request(Command::AddProject { completion_policy, .. }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-policy", "soft",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_policy, Some(CompletionPolicy::Soft));
    }

    #[test]
    fn given_completion_policy_bogus_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-policy", "bogus",
        ]))
        .is_err());
    }

    #[test]
    fn given_completion_policy_uppercase_when_parsed_then_err() {
        // from_str is exact lowercase; `AUTO` is not a known variant.
        assert!(parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-policy", "AUTO",
        ]))
        .is_err());
    }

    #[test]
    fn given_completion_policy_empty_value_via_equals_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-policy=",
        ]))
        .is_err());
    }

    #[test]
    fn given_completion_policy_equals_form_when_parsed_then_some_auto() {
        let CliAction::Request(Command::AddProject { completion_policy, .. }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-policy=auto",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_policy, Some(CompletionPolicy::Auto));
    }

    #[test]
    fn given_plan_gate_timeout_zero_when_parsed_then_some_zero() {
        let CliAction::Request(Command::AddProject { plan_gate_timeout_min, .. }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--plan-gate-timeout", "0",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(plan_gate_timeout_min, Some(0));
    }

    #[test]
    fn given_plan_gate_timeout_fifteen_when_parsed_then_some_fifteen() {
        let CliAction::Request(Command::AddProject { plan_gate_timeout_min, .. }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--plan-gate-timeout", "15",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(plan_gate_timeout_min, Some(15));
    }

    #[test]
    fn given_plan_gate_timeout_non_integer_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--plan-gate-timeout", "notanint",
        ]))
        .is_err());
    }

    #[test]
    fn given_negative_plan_gate_timeout_when_parsed_then_some_negative() {
        // `-5` is taken as the flag value (not another flag) and i64 accepts it;
        // no sign validation at the parse layer.
        let CliAction::Request(Command::AddProject { plan_gate_timeout_min, .. }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--plan-gate-timeout", "-5",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(plan_gate_timeout_min, Some(-5));
    }

    #[test]
    fn given_bare_plan_gate_timeout_flag_at_end_when_parsed_then_none() {
        // A `--key` with no following value is classified as a boolean flag, so
        // the optional int reads as absent (None), not an error. ^ pins this.
        let CliAction::Request(Command::AddProject { plan_gate_timeout_min, .. }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--plan-gate-timeout",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(plan_gate_timeout_min, None);
    }

    #[test]
    fn given_completion_timeout_thirty_when_parsed_then_some_thirty() {
        let CliAction::Request(Command::AddProject { completion_soft_timeout_min, .. }) =
            parse(&argv(&[
                "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
                "--completion-timeout", "30",
            ]))
            .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_soft_timeout_min, Some(30));
    }

    #[test]
    fn given_completion_timeout_non_integer_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-timeout", "abc",
        ]))
        .is_err());
    }

    #[test]
    fn given_large_completion_timeout_when_parsed_then_some_i64_max() {
        let CliAction::Request(Command::AddProject { completion_soft_timeout_min, .. }) =
            parse(&argv(&[
                "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
                "--completion-timeout", "9223372036854775807",
            ]))
            .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_soft_timeout_min, Some(i64::MAX));
    }

    #[test]
    fn given_all_three_new_flags_when_parsed_then_addproject_carries_each() {
        assert_eq!(
            parse(&argv(&[
                "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
                "--completion-policy", "soft", "--plan-gate-timeout", "0", "--completion-timeout",
                "45"
            ]))
            .unwrap(),
            CliAction::Request(Command::AddProject {
                name: "demo".to_string(),
                repo_path: "/repo".to_string(),
                default_branch: "main".to_string(),
                main_agent_cmd: "mc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: None,
                completion_policy: Some(CompletionPolicy::Soft),
                plan_gate_timeout_min: Some(0),
                completion_soft_timeout_min: Some(45),
            })
        );
    }

    #[test]
    fn given_completion_timeout_overflow_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-timeout", "9223372036854775808",
        ]))
        .is_err());
    }

    #[test]
    fn given_plan_gate_timeout_overflow_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--plan-gate-timeout", "99999999999999999999",
        ]))
        .is_err());
    }

    #[test]
    fn given_completion_policy_repeated_when_parsed_then_last_wins_soft() {
        let CliAction::Request(Command::AddProject { completion_policy, .. }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-policy", "auto", "--completion-policy", "soft",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_policy, Some(CompletionPolicy::Soft));
    }

    #[test]
    fn given_completion_policy_whitespace_value_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-policy", " ",
        ]))
        .is_err());
    }

    #[test]
    fn given_plan_gate_timeout_equals_form_zero_when_parsed_then_some_zero() {
        let CliAction::Request(Command::AddProject { plan_gate_timeout_min, .. }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--plan-gate-timeout=0",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(plan_gate_timeout_min, Some(0));
    }

    #[test]
    fn given_policy_only_flag_when_parsed_then_timeouts_stay_none() {
        let CliAction::Request(Command::AddProject {
            plan_gate_timeout_min,
            completion_soft_timeout_min,
            ..
        }) = parse(&argv(&[
            "project", "add", "demo", "/repo", "--main", "mc", "--plan", "pc", "--work", "wc",
            "--completion-policy", "auto",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!((plan_gate_timeout_min, completion_soft_timeout_min), (None, None));
    }
}

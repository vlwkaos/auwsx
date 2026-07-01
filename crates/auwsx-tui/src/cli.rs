//! `auwsx` command-line surface: a thin IPC client plus the daemon entry point.
//!
//! [`parse`] is pure (args in, [`CliAction`] out) so the whole arg grammar is
//! unit-testable without a socket; [`run_daemon`] / [`run_request`] are the
//! runtime glue. Every non-daemon subcommand becomes one [`Command`] sent to the
//! running daemon over the Unix socket — the daemon owns all DB writes.

use anyhow::{bail, Context, Result};
use auwsx_core::backlog::{Approval, Source};
use auwsx_core::control_outbox;
use auwsx_core::db::ask_answers::AskMode;
use auwsx_core::db::findings::Severity;
use auwsx_core::db::projects::CompletionPolicy;
use auwsx_core::db::remote::{RemoteAuthKind, RemoteProvider, RequiredChecksPolicy};
use auwsx_core::events;
use auwsx_core::ipc::{self, Command, Response};
use auwsx_core::remote_plan::{RemoteCommentTarget, RemotePlannedAction};
use auwsx_core::state::IssueStatus;
use auwsx_core::steering::SteeringSource;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// What the parsed argv asks `auwsx` to do.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum CliAction {
    /// Run the daemon (IPC server) in the foreground.
    Daemon,
    /// Send one command to the running daemon and print the reply.
    Request(Command),
    /// Local git maintenance that must also work after the DB has been reset.
    PruneWorktrees { repo_path: String },
    /// Print usage.
    Help,
    /// No recognized subcommand — caller falls through to the TUI.
    Tui,
}

const USAGE: &str = "\
auwsx — autonomous workspace orchestrator

USAGE:
  auwsx                         launch the TUI and auto-start the daemon
  auwsx daemon                  run the daemon (IPC server) in the foreground
  auwsx daemon stop             ask the running daemon to shut down
  auwsx ping                    check the daemon is up
  auwsx settings                print global settings without launching the TUI
  auwsx worktree prune <repo_path>  remove orphaned auwsx issue worktrees
  auwsx scheduler run <project_id>  execute one scheduler pass now

  auwsx arsenal ls
  auwsx arsenal set <name> --main <cmd> --plan <cmd> --work <cmd> [--review <cmd>]
  auwsx ask <project_id> [--mode recall|seek] <question...>
  auwsx ask ls <project_id>
  auwsx memory retrieve <project_id> <query...>
  auwsx memory save <project_id> --kind <kind> (--file <path> | <text...>)
  auwsx memory consolidate <project_id> --mode dream|deepsleep
  auwsx memory preset ls
  auwsx memory preset set <name> \\
        --retrieve-kind portable|command|auwsx_skill [--retrieve-cmd <cmd>] \\
        --save-kind portable|command|auwsx_skill [--save-cmd <cmd>] \\
        --dream-kind portable|command|auwsx_skill [--dream-cmd <cmd>] \\
        --deepsleep-kind portable|command|auwsx_skill [--deepsleep-cmd <cmd>]

  auwsx project add <name> <repo_path> --branch <b> \\
        (--arsenal <preset> | --main <cmd> --plan <cmd> --work <cmd>) [--route <cmd>] [--review <cmd>] \\
        [--completion-policy manual|soft|auto] \\
        [--plan-gate-timeout <min>] [--merge-delay <min>] [--schedule <cron|30m|1h|1d>]
  auwsx project ls
  auwsx project get <project_id>           show one project incl. resolved policy
  auwsx project merge <project_id>         approve all READY_TO_MERGE issues
  auwsx project diagnose <project_id>      inspect merge/worktree recovery state
  auwsx project reconcile <project_id> [--dry-run]  apply safe recovery, queue hard cases
  auwsx project remote get <project_id>
  auwsx project remote set <project_id> --url <url> --owner <owner> --repo <repo> \\
        [--api-base-url <url>] [--auth-kind none|token_env|github_app] [--auth-ref <ref>] \\
        [--webhook-secret-ref <ref>] [--inbound-auwsx-run] [--outbound-issue-create] \\
        [--remote-pr-merge] [--agent-comments] [--subtask-comments] [--finding-comments] \\
        [--draft-pr] [--required-checks observe|require_green] [--labels <csv>] \\
        [--assignees <csv>] [--pr-base <branch>]
  auwsx project remote delete <project_id>
  auwsx project remote sync-runs <project_id> [--limit <n>]
  auwsx project remote plan <issue_id>
  auwsx reconcile apply <main_job_id>      apply a validated reconcile proposal

  auwsx backlog add <project_id> <text...> [--source human|agent|routine|inbox]
  auwsx backlog ls <project_id> [--approval pending|approved|dismissed]
  auwsx backlog approve <item_id>
  auwsx backlog dismiss <item_id>

  auwsx issue ls <project_id> [--status <STATUS>]
  auwsx issue get <issue_id>
  auwsx issue status <issue_id> <STATUS> [--force]
  auwsx issue retry <issue_id>             retry a FAILED issue
  auwsx issue merge <issue_id>             approve one READY_TO_MERGE issue
  auwsx issue apply-merge <issue_id>       run deterministic local merge transaction
  auwsx issue abandon <issue_id>
  auwsx issue cleanup <issue_id>       remove this issue's worktree only

  auwsx finding add <issue_id> <round> <severity> <title...> \\
        [--lens <l>] [--detail <d>] [--file <ref>]
  auwsx finding ls <issue_id> [--open]
  auwsx finding accept <finding_id> <rationale...>
  auwsx finding reject <finding_id> <rationale...>
  auwsx finding dismiss <finding_id>

  auwsx queue add <issue_id> <source> <note...>     (source: human|consolidation)
  auwsx queue ls <issue_id>
  auwsx queue consume <issue_id>
";

/// Parse argv (without the program name) into a [`CliAction`]. Pure: no IO.
pub fn parse(args: &[String]) -> Result<CliAction> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Ok(CliAction::Tui);
    };
    match sub {
        "help" | "--help" | "-h" => Ok(CliAction::Help),
        "ping" => Ok(CliAction::Request(Command::Ping)),
        "settings" | "config" => Ok(CliAction::Request(Command::GetGlobalSettings)),
        "daemon" => match args.get(1).map(String::as_str) {
            None => Ok(CliAction::Daemon),
            Some("stop") => Ok(CliAction::Request(Command::Shutdown)),
            Some(other) => bail!("unknown `daemon` subcommand: {other}"),
        },
        "arsenal" => parse_arsenal(&args[1..]),
        "worktree" => parse_worktree(&args[1..]),
        "scheduler" => parse_scheduler(&args[1..]),
        "ask" => parse_ask(&args[1..]),
        "memory" => parse_memory(&args[1..]),
        "project" => parse_project(&args[1..]),
        "reconcile" => parse_reconcile(&args[1..]),
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
        "queue" | "steering" => parse_queue(&args[1..]),
        // Unknown leading token: let the caller decide (TUI fallback).
        _ => Ok(CliAction::Tui),
    }
}

fn parse_scheduler(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "scheduler")?;
    let p = Parsed::new(rest);
    match verb {
        "run" | "once" => Ok(CliAction::Request(Command::RunSchedulerOnce {
            project_id: p.int(0, "project_id")?,
        })),
        other => bail!("unknown `scheduler` subcommand: {other}"),
    }
}

fn parse_worktree(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "worktree")?;
    let p = Parsed::new(rest);
    match verb {
        "prune" => Ok(CliAction::PruneWorktrees {
            repo_path: p.pos(0, "repo_path")?,
        }),
        other => bail!("unknown `worktree` subcommand: {other}"),
    }
}

fn parse_ask(args: &[String]) -> Result<CliAction> {
    if matches!(args.first().map(String::as_str), Some("ls" | "list")) {
        let p = Parsed::new(&args[1..]);
        return Ok(CliAction::Request(Command::ListAskAnswers {
            project_id: p.int(0, "project_id")?,
            limit: 20,
        }));
    }
    let p = Parsed::new(args);
    let mode = match p.flag("mode").as_deref().unwrap_or("recall") {
        "recall" => AskMode::Recall,
        "seek" => AskMode::Seek,
        other => bail!("invalid ask --mode {other:?}"),
    };
    Ok(CliAction::Request(Command::AskProject {
        project_id: p.int(0, "project_id")?,
        mode,
        question: p.rest_from(1, "question")?,
    }))
}

fn parse_memory(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "memory")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "preset" => return parse_memory_preset(rest),
        "retrieve" | "recall" => Command::MemoryRetrieve {
            project_id: p.int(0, "project_id")?,
            query: p.rest_from(1, "query")?,
        },
        "save" => {
            let project_id = p.int(0, "project_id")?;
            let kind = p.flag("kind").unwrap_or_else(|| "note".to_string());
            let content = if let Some(path) = p.flag("file") {
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading memory save file {path}"))?
            } else {
                p.rest_from(1, "text")?
            };
            Command::MemorySave {
                project_id,
                kind,
                content,
            }
        }
        "consolidate" => Command::MemoryConsolidate {
            project_id: p.int(0, "project_id")?,
            mode: p.flag("mode").unwrap_or_else(|| "deepsleep".to_string()),
        },
        other => bail!("unknown `memory` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_memory_preset(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "memory preset")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "ls" | "list" => Command::ListMemoryPresets,
        "set" => {
            p.exact_positionals(1, "memory preset set")?;
            Command::UpsertMemoryPreset {
                name: p.pos(0, "name")?,
                retrieve_kind: p.req_flag("retrieve-kind")?,
                retrieve_cmd: p.flag("retrieve-cmd"),
                save_kind: p.req_flag("save-kind")?,
                save_cmd: p.flag("save-cmd"),
                dream_kind: p.req_flag("dream-kind")?,
                dream_cmd: p.flag("dream-cmd"),
                deepsleep_kind: p.req_flag("deepsleep-kind")?,
                deepsleep_cmd: p.flag("deepsleep-cmd"),
            }
        }
        other => bail!("unknown `memory preset` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_arsenal(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "arsenal")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "ls" | "list" => Command::ListArsenalPresets,
        "set" => {
            p.exact_positionals(1, "arsenal set")?;
            let work_agent_cmd = p.req_flag("work")?;
            Command::UpsertArsenalPreset {
                name: p.pos(0, "name")?,
                main_agent_cmd: p.req_flag("main")?,
                route_agent_cmd: p.flag("route").unwrap_or_else(|| work_agent_cmd.clone()),
                plan_agent_cmd: p.req_flag("plan")?,
                work_agent_cmd,
                review_agent_cmd: p.flag("review"),
            }
        }
        other => bail!("unknown `arsenal` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_reconcile(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "reconcile")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "apply" => {
            p.exact_positionals(1, "reconcile apply")?;
            p.exact_flags(&[], "reconcile apply")?;
            Command::ApplyReconcile {
                main_job_id: p.int(0, "main_job_id")?,
            }
        }
        other => bail!("unknown `reconcile` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_project(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "project")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "add" => {
            p.exact_positionals(2, "project add")?;
            let arsenal_preset_name = p.flag("arsenal");
            let has_arsenal = arsenal_preset_name.is_some();
            let schedule_cron = match p.flag("schedule-cron").or_else(|| p.flag("schedule")) {
                Some(raw) => auwsx_core::schedule::normalize_cadence_input(&raw)
                    .with_context(|| format!("invalid --schedule {raw:?}"))?,
                None => None,
            };
            let work_agent_cmd = if has_arsenal {
                p.flag("work").unwrap_or_default()
            } else {
                p.req_flag("work")?
            };
            Command::AddProject {
                name: p.pos(0, "name")?,
                repo_path: p.pos(1, "repo_path")?,
                default_branch: p.flag("branch").unwrap_or_else(|| "main".to_string()),
                arsenal_preset_name,
                main_agent_cmd: if has_arsenal {
                    p.flag("main").unwrap_or_default()
                } else {
                    p.req_flag("main")?
                },
                route_agent_cmd: if has_arsenal {
                    p.flag("route").unwrap_or_default()
                } else {
                    p.flag("route").unwrap_or_else(|| work_agent_cmd.clone())
                },
                plan_agent_cmd: if has_arsenal {
                    p.flag("plan").unwrap_or_default()
                } else {
                    p.req_flag("plan")?
                },
                work_agent_cmd,
                review_agent_cmd: p.flag("review"),
                completion_policy: match p.flag("completion-policy") {
                    Some(s) => Some(
                        CompletionPolicy::from_str(&s)
                            .with_context(|| format!("invalid --completion-policy {s:?}"))?,
                    ),
                    None => None,
                },
                plan_gate_timeout_min: p.opt_int("plan-gate-timeout")?,
                completion_soft_timeout_min: p
                    .opt_int_any(&["merge-delay", "completion-timeout"])?,
                schedule_interval_min: p.opt_int("schedule-min")?,
                schedule_cron,
            }
        }
        "ls" | "list" => Command::ListProjects,
        "get" => Command::GetProject {
            project_id: p.int(0, "project_id")?,
        },
        "merge" => Command::ApproveProjectMerge {
            project_id: p.int(0, "project_id")?,
        },
        "diagnose" => {
            p.exact_positionals(1, "project diagnose")?;
            p.exact_flags(&[], "project diagnose")?;
            Command::DiagnoseProject {
                project_id: p.int(0, "project_id")?,
            }
        }
        "reconcile" => {
            p.exact_positionals(1, "project reconcile")?;
            p.exact_bool_flags(&["dry-run"], "project reconcile")?;
            Command::ReconcileProject {
                project_id: p.int(0, "project_id")?,
                dry_run: p.has("dry-run"),
            }
        }
        "remote" => return parse_project_remote(rest),
        other => bail!("unknown `project` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_project_remote(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "project remote")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "get" => {
            p.exact_positionals(1, "project remote get")?;
            p.exact_flags(&[], "project remote get")?;
            Command::GetProjectRemoteConfig {
                project_id: p.int(0, "project_id")?,
            }
        }
        "set" => {
            p.exact_positionals(1, "project remote set")?;
            p.exact_flags(
                &[
                    "provider",
                    "url",
                    "owner",
                    "repo",
                    "api-base-url",
                    "auth-kind",
                    "auth-ref",
                    "webhook-secret-ref",
                    "inbound-auwsx-run",
                    "outbound-issue-create",
                    "remote-pr-merge",
                    "agent-comments",
                    "subtask-comments",
                    "finding-comments",
                    "draft-pr",
                    "required-checks",
                    "labels",
                    "assignees",
                    "pr-base",
                ],
                "project remote set",
            )?;
            let provider = match p.flag("provider").as_deref().unwrap_or("github") {
                "github" => RemoteProvider::Github,
                other => bail!("invalid --provider {other:?}"),
            };
            let auth_kind = match p.flag("auth-kind").as_deref().unwrap_or("token_env") {
                "none" => RemoteAuthKind::None,
                "token_env" => RemoteAuthKind::TokenEnv,
                "github_app" => RemoteAuthKind::GithubApp,
                other => bail!("invalid --auth-kind {other:?}"),
            };
            let required_checks = match p.flag("required-checks").as_deref().unwrap_or("observe") {
                "observe" => RequiredChecksPolicy::Observe,
                "require_green" => RequiredChecksPolicy::RequireGreen,
                other => bail!("invalid --required-checks {other:?}"),
            };
            Command::UpsertProjectRemoteConfig {
                project_id: p.int(0, "project_id")?,
                provider,
                remote_url: p.req_flag("url")?,
                owner: p.req_flag("owner")?,
                repo: p.req_flag("repo")?,
                api_base_url: p
                    .flag("api-base-url")
                    .unwrap_or_else(|| "https://api.github.com".to_string()),
                auth_kind,
                auth_ref: p.flag("auth-ref"),
                webhook_secret_ref: p.flag("webhook-secret-ref"),
                inbound_auwsx_run_enabled: p.has("inbound-auwsx-run"),
                outbound_issue_create_enabled: p.has("outbound-issue-create"),
                remote_pr_merge_enabled: p.has("remote-pr-merge"),
                agent_comment_sync_enabled: p.has("agent-comments"),
                subtask_comment_sync_enabled: p.has("subtask-comments"),
                finding_comment_sync_enabled: p.has("finding-comments"),
                draft_pr_enabled: p.has("draft-pr"),
                required_checks_policy: required_checks,
                default_labels: p.flag("labels"),
                default_assignees: p.flag("assignees"),
                pr_base_branch: p.flag("pr-base"),
            }
        }
        "delete" | "rm" => {
            p.exact_positionals(1, "project remote delete")?;
            p.exact_flags(&[], "project remote delete")?;
            Command::DeleteProjectRemoteConfig {
                project_id: p.int(0, "project_id")?,
            }
        }
        "sync-runs" => {
            p.exact_positionals(1, "project remote sync-runs")?;
            p.exact_flags(&["limit"], "project remote sync-runs")?;
            Command::RecentRemoteSyncRuns {
                project_id: p.int(0, "project_id")?,
                limit: p.opt_int("limit")?.unwrap_or(20),
            }
        }
        "plan" => {
            p.exact_positionals(1, "project remote plan")?;
            p.exact_flags(&[], "project remote plan")?;
            Command::PlanIssueRemoteWorkflow {
                issue_id: p.int(0, "issue_id")?,
            }
        }
        other => bail!("unknown `project remote` subcommand: {other}"),
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
                Some(s) => {
                    Source::from_str(&s).with_context(|| format!("invalid --source {s:?}"))?
                }
                None => Source::Human,
            },
        },
        "ls" | "list" => Command::ListBacklog {
            project_id: p.int(0, "project_id")?,
            approval: match p.flag("approval") {
                Some(a) => Some(
                    Approval::from_str(&a).with_context(|| format!("invalid --approval {a:?}"))?,
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
        "add" => bail!("issues are agent-derived; add backlog instead"),
        "ls" | "list" => Command::ListIssues {
            project_id: p.int(0, "project_id")?,
            status: match p.flag("status") {
                Some(s) => Some(
                    IssueStatus::from_str(&s).with_context(|| format!("invalid --status {s:?}"))?,
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
        "retry" => Command::RetryIssue {
            issue_id: p.int(0, "issue_id")?,
        },
        "merge" => Command::ApproveIssueMerge {
            issue_id: p.int(0, "issue_id")?,
        },
        "apply-merge" => Command::ApplyIssueMerge {
            issue_id: p.int(0, "issue_id")?,
        },
        "absorb" => Command::AbsorbIssue {
            issue_id: p.int(0, "issue_id")?,
            into_issue_id: p.int(1, "into_issue_id")?,
        },
        "abandon" => Command::AbandonIssue {
            issue_id: p.int(0, "issue_id")?,
        },
        "cleanup" => Command::CleanupIssueWorktree {
            issue_id: p.int(0, "issue_id")?,
        },
        "remove" | "rm" => Command::RemoveIssue {
            issue_id: p.int(0, "issue_id")?,
        },
        other => bail!("unknown `issue` subcommand: {other}"),
    };
    Ok(CliAction::Request(cmd))
}

fn parse_subtask(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "subtask")?;
    let p = Parsed::new(rest);
    let cmd = match verb {
        "add" => {
            if std::env::var_os(control_outbox::OUTBOX_ENV).is_none() {
                bail!("subtasks are agent-derived and must be added by issue agents");
            }
            Command::AddSubtask {
                issue_id: p.int(0, "issue_id")?,
                ord: p.int(1, "ord")?,
                text: p.rest_from(2, "text")?,
            }
        }
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

fn parse_queue(args: &[String]) -> Result<CliAction> {
    let (verb, rest) = split_verb(args, "queue")?;
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
        other => bail!("unknown `queue` subcommand: {other}"),
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

    fn exact_positionals(&self, expected: usize, context: &str) -> Result<()> {
        if self.positionals.len() != expected {
            bail!(
                "`{context}` expects {expected} positional argument(s), got {}. Quote multi-word option values.",
                self.positionals.len()
            );
        }
        Ok(())
    }

    fn exact_flags(&self, allowed: &[&str], context: &str) -> Result<()> {
        for key in self.flags.keys() {
            if !allowed.iter().any(|allowed| allowed == key) {
                bail!("`{context}` does not accept --{key}");
            }
        }
        for key in &self.bools {
            if !allowed.iter().any(|allowed| allowed == key) {
                bail!("`{context}` does not accept --{key}");
            }
        }
        Ok(())
    }

    fn exact_bool_flags(&self, allowed: &[&str], context: &str) -> Result<()> {
        if let Some(key) = self.flags.keys().next() {
            bail!("`{context}` option --{key} does not take a value");
        }
        self.exact_flags(allowed, context)
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

    fn opt_int_any(&self, keys: &[&str]) -> Result<Option<i64>> {
        for key in keys {
            if self.flag(key).is_some() {
                return self.opt_int(key);
            }
        }
        Ok(None)
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
    let scheduler = Arc::new(Scheduler::new(
        db.clone(),
        clock::system(),
        subprocess_executor(),
        Arc::new(WsxWorktrees),
        bus.clone(),
        socket.clone(),
        Duration::from_secs(tick_secs),
    ));
    let recovered = scheduler
        .recover_interrupted_work(chrono::Utc::now().timestamp_millis())
        .await
        .context("recovering interrupted work")?;
    if recovered > 0 {
        eprintln!("recovered {recovered} interrupted work record(s) from previous daemon exit");
    }
    let sched_stop = Arc::new(Notify::new());
    let sched_stop_run = sched_stop.clone();
    let scheduler_run = scheduler.clone();
    let sched_task = tokio::spawn(async move { scheduler_run.run(sched_stop_run).await });

    println!("auwsx daemon listening on {}", socket.display());
    println!("scheduler ticking every {tick_secs}s");
    let serve_result = ipc::serve_with_scheduler(db, bus, &socket, shutdown, scheduler).await;

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
    if let Some(resp) = auwsx_core::control_outbox::handle_local_command(&cmd)? {
        if print_response(resp) {
            std::process::exit(1);
        }
        return Ok(());
    }

    let socket = ipc::default_socket_path();
    if !matches!(cmd, Command::Shutdown) {
        ensure_daemon(&socket).await?;
    }
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

/// Prune auwsx-managed issue worktrees that no longer match DB state.
///
/// This is intentionally local rather than daemon-owned: after a DB reset there
/// may be no project rows left, but git's worktree registry can still contain
/// `auwsx/issue-*` entries that would block future issue ids.
pub async fn run_prune_worktrees(repo_path: String) -> Result<()> {
    use auwsx_core::db::issues;
    use auwsx_core::db::projects;
    use auwsx_core::db::Db;
    use auwsx_core::worktree::prune_orphaned_issue_worktrees;

    let repo = PathBuf::from(repo_path);
    let db = Db::open().await.context("opening database")?;
    let mut known = HashMap::new();
    for project in projects::list(db.pool()).await? {
        if !same_path(Path::new(&project.repo_path), &repo) {
            continue;
        }
        for issue in issues::list_by_project(db.pool(), project.id).await? {
            if let Some(path) = issue.worktree_path {
                known.insert(issue.id, PathBuf::from(path));
            }
        }
    }
    let removed = prune_orphaned_issue_worktrees(&repo, &known).await?;
    if removed.is_empty() {
        println!("no orphaned auwsx issue worktrees");
    } else {
        for handle in &removed {
            println!("removed {}\t{}", handle.branch, handle.path.display());
        }
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(l), Ok(r)) => l == r,
        _ => left == right,
    }
}

pub async fn ensure_daemon(socket: &std::path::Path) -> Result<()> {
    if ipc::request(socket, &Command::Ping).await.is_ok() {
        return Ok(());
    }

    let _lock = acquire_daemon_start_lock(socket).await?;
    if ipc::request(socket, &Command::Ping).await.is_ok() {
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting auwsx daemon")?;

    let mut last_err = None;
    for _ in 0..50 {
        match ipc::request(socket, &Command::Ping).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if let Some(status) = child.try_wait().context("checking auwsx daemon startup")? {
                    anyhow::bail!("daemon exited before becoming ready with status {status}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    match last_err {
        Some(e) => Err(e).context("daemon did not become ready"),
        None => anyhow::bail!("daemon did not become ready"),
    }
}

struct DaemonStartLock {
    path: PathBuf,
    _file: File,
}

impl Drop for DaemonStartLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

async fn acquire_daemon_start_lock(socket: &std::path::Path) -> Result<DaemonStartLock> {
    let lock_path = socket.with_extension("start.lock");
    if let Some(parent) = lock_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating daemon socket directory {}", parent.display()))?;
    }
    let started = Instant::now();
    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => {
                return Ok(DaemonStartLock {
                    path: lock_path,
                    _file: file,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&lock_path, Duration::from_secs(15)) {
                    let _ = fs::remove_file(&lock_path);
                    continue;
                }
                if started.elapsed() > Duration::from_secs(20) {
                    anyhow::bail!(
                        "timed out waiting for daemon startup lock {}",
                        lock_path.display()
                    );
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("creating daemon startup lock {}", lock_path.display())
                });
            }
        }
    }
}

fn lock_is_stale(path: &Path, max_age: Duration) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    modified.elapsed().is_ok_and(|age| age > max_age)
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
        Response::AskAnswers(answers) => {
            for answer in answers {
                println!(
                    "{}\t{}\t{}\nQ: {}\nA: {}\n",
                    answer.id,
                    answer.mode.as_str(),
                    answer.created_at,
                    answer.question,
                    answer.answer
                );
            }
        }
        Response::ArsenalPresets(presets) => {
            for p in presets {
                let kind = if p.builtin { "builtin" } else { "custom" };
                println!(
                    "{}\t{}\tmain={}\troute={}\tplan={}\twork={}\treview={}",
                    p.name,
                    kind,
                    p.main_agent_cmd,
                    p.route_agent_cmd,
                    p.plan_agent_cmd,
                    p.work_agent_cmd,
                    p.review_agent_cmd
                        .as_deref()
                        .unwrap_or("(falls back to work)")
                );
            }
        }
        Response::MemoryPresets(presets) => {
            for p in presets {
                let scope = if p.builtin { "builtin" } else { "custom" };
                println!(
                    "{}\t{}\tretrieve={} {}\tsave={} {}\tdream={} {}\tdeepsleep={} {}",
                    p.name,
                    scope,
                    p.retrieve_kind,
                    p.retrieve_cmd.as_deref().unwrap_or(""),
                    p.save_kind,
                    p.save_cmd.as_deref().unwrap_or(""),
                    p.dream_kind,
                    p.dream_cmd.as_deref().unwrap_or(""),
                    p.deepsleep_kind,
                    p.deepsleep_cmd.as_deref().unwrap_or("")
                );
            }
        }
        Response::Profiles(profiles) => {
            for p in profiles {
                println!("{}\t{}\t{}", p.id, p.ord, p.name);
            }
        }
        Response::GlobalSettings(settings) => {
            println!("memory_preset: {}", settings.memory_preset_name);
            println!("pipeline_ux_guidance:");
            println!("{}", printable_multiline(&settings.pipeline_ux_guidance));
        }
        Response::Projects(ps) => {
            for p in ps {
                println!(
                    "{}\t{}\t{}\t{}",
                    p.id, p.name, p.default_branch, p.repo_path
                );
            }
        }
        Response::Project(Some(p)) => {
            println!("id:     {}", p.id);
            println!("name:   {}", p.name);
            println!("repo:   {}", p.repo_path);
            println!("branch: {}", p.default_branch);
            println!(
                "arsenal: {}",
                p.arsenal_preset_name.as_deref().unwrap_or("(custom)")
            );
            println!(
                "agents: main={} route={} plan={} work={}",
                p.main_agent_cmd, p.route_agent_cmd, p.plan_agent_cmd, p.work_agent_cmd
            );
            println!(
                "policy: completion={} plan_gate={}min completion_soft={}min concurrency={}",
                p.completion_policy.as_str(),
                p.plan_gate_timeout_min,
                p.completion_soft_timeout_min,
                p.max_concurrency
            );
        }
        Response::Project(None) => println!("not found"),
        Response::ProjectRemoteConfig(Some(c)) => {
            println!("project: {}", c.project_id);
            println!("provider: {}", c.provider.as_str());
            println!("remote: {}", c.remote_url);
            println!("repo: {}/{}", c.owner, c.repo);
            println!("api: {}", c.api_base_url);
            println!(
                "auth: {} {}",
                c.auth_kind.as_str(),
                redact_opt(c.auth_ref.as_deref())
            );
            println!(
                "webhook_secret: {}",
                redact_opt(c.webhook_secret_ref.as_deref())
            );
            println!(
                "toggles: inbound_auwsx_run={} outbound_issue_create={} remote_pr_merge={} agent_comments={} subtask_comments={} finding_comments={} draft_pr={}",
                c.inbound_auwsx_run_enabled,
                c.outbound_issue_create_enabled,
                c.remote_pr_merge_enabled,
                c.agent_comment_sync_enabled,
                c.subtask_comment_sync_enabled,
                c.finding_comment_sync_enabled,
                c.draft_pr_enabled
            );
            println!("required_checks: {}", c.required_checks_policy.as_str());
            println!(
                "defaults: labels={} assignees={} pr_base={}",
                c.default_labels.as_deref().unwrap_or("-"),
                c.default_assignees.as_deref().unwrap_or("-"),
                c.pr_base_branch.as_deref().unwrap_or("-")
            );
        }
        Response::ProjectRemoteConfig(None) => println!("not configured"),
        Response::ReconcileReport(report) => {
            println!(
                "project {}\tdry_run={}\tsafe={}\tmanual={}\tagentic={}\tapplied={}\tqueued={}",
                report.project_id,
                report.dry_run,
                report.safe_count,
                report.manual_count,
                report.agentic_count,
                report.applied_count,
                report
                    .queued_main_job_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "-".to_string())
            );
            for issue in report.issues {
                println!(
                    "issue\t{}\t{}\t{}\t{}\t{}\t{}",
                    issue.issue_id,
                    issue.status.as_str(),
                    issue.diagnosis.as_str(),
                    issue.proposed_action.as_str(),
                    issue.confidence,
                    issue.blocking_reason.unwrap_or_default()
                );
            }
            for orphan in report.orphans {
                println!(
                    "orphan\t{}\t{}\t{}\t{}",
                    orphan.issue_id,
                    orphan.diagnosis.as_str(),
                    orphan.proposed_action.as_str(),
                    orphan.path
                );
            }
        }
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
        Response::Routines(rs) => {
            for r in rs {
                let enabled = if r.enabled { "on" } else { "off" };
                println!("{}\t{}\t{}\t{}", r.id, enabled, r.cron, r.name);
            }
        }
        Response::Routine(Some(r)) => {
            println!("id:      {}", r.id);
            println!("name:    {}", r.name);
            println!("enabled: {}", r.enabled);
            println!("output:  {}", r.output_route.as_str());
            println!("cron:    {}", r.cron);
            println!("prompt:  {}", r.prompt);
        }
        Response::Routine(None) => println!("not found"),
        Response::AgentRuns(runs) => {
            for r in runs {
                println!(
                    "{}\t{:?}\t{}\t{}\t{}",
                    r.id,
                    r.issue_id.or(r.main_job_id),
                    r.role.as_str(),
                    r.phase,
                    r.log_path.unwrap_or_default()
                );
            }
        }
        Response::MainJobs(jobs) => {
            for j in jobs {
                println!(
                    "{}\t{:?}\t{}\t{}",
                    j.id,
                    j.status,
                    j.kind,
                    j.outcome.unwrap_or_default()
                );
            }
        }
        Response::SchedulerRuns(runs) => {
            for r in runs {
                println!(
                    "{}\t{}\t{}\t{}",
                    r.id,
                    r.source.as_str(),
                    r.fired_at,
                    r.picked.unwrap_or_default()
                );
            }
        }
        Response::RemoteSyncRuns(runs) => {
            for r in runs {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    r.id,
                    r.direction.as_str(),
                    r.kind.as_str(),
                    r.status.as_str(),
                    r.summary.unwrap_or_default()
                );
            }
        }
        Response::IssueRemoteWorkflowPlan(plan) => {
            println!(
                "actions: {}\tblockers: {}",
                plan.actions.len(),
                plan.blockers.len()
            );
            for action in plan.actions {
                match action {
                    RemotePlannedAction::CreateIssue {
                        issue_id,
                        title,
                        labels,
                        assignees,
                        ..
                    } => println!(
                        "action\tcreate_issue\tissue={issue_id}\ttitle={title}\tlabels={}\tassignees={}",
                        labels.join(","),
                        assignees.join(",")
                    ),
                    RemotePlannedAction::CreateOrUpdatePullRequest {
                        issue_id,
                        title,
                        head_branch,
                        base_branch,
                        draft,
                        require_green_checks,
                        ..
                    } => println!(
                        "action\tcreate_or_update_pr\tissue={issue_id}\ttitle={title}\thead={head_branch}\tbase={base_branch}\tdraft={draft}\trequire_green={require_green_checks}"
                    ),
                    RemotePlannedAction::PostProgressComment {
                        issue_id,
                        target,
                        remote_link_id,
                        marker,
                        ..
                    } => {
                        let target = match target {
                            RemoteCommentTarget::Issue => "issue",
                            RemoteCommentTarget::PullRequest => "pr",
                        };
                        println!(
                            "action\tpost_progress_comment\tissue={issue_id}\ttarget={target}\tlink={remote_link_id}\tmarker={marker}"
                        );
                    }
                }
            }
            for blocker in plan.blockers {
                println!("blocker\t{blocker:?}");
            }
        }
        Response::RemoteInboundOutcome(outcome) => println!("{outcome:?}"),
        Response::LogTail { path, text } => {
            println!("==> {path}");
            print!("{text}");
        }
        Response::MemoryText { text } => println!("{}", printable_multiline(&text)),
        Response::RanIssue { issue_id } => println!("running issue {issue_id}"),
        Response::ApprovedMerge { issue_ids } => {
            let joined = issue_ids
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            println!("approved merge issues {joined}");
        }
        Response::Event(ev) => println!("event: {ev:?}"),
    }
    false
}

fn printable_multiline(value: &str) -> String {
    value
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
        .collect()
}

fn redact_opt(value: Option<&str>) -> &str {
    if value.is_some() {
        "(set)"
    } else {
        "-"
    }
}

/// Print top-level usage.
pub fn print_usage() {
    print!("{USAGE}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static CONTROL_OUTBOX_ENV_LOCK: Mutex<()> = Mutex::new(());

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

    #[test]
    fn given_settings_when_parsed_then_request_global_settings() {
        assert_eq!(
            parse(&argv(&["settings"])).unwrap(),
            CliAction::Request(Command::GetGlobalSettings)
        );
    }

    #[test]
    fn given_config_alias_when_parsed_then_request_global_settings() {
        assert_eq!(
            parse(&argv(&["config"])).unwrap(),
            CliAction::Request(Command::GetGlobalSettings)
        );
    }

    #[test]
    fn given_worktree_prune_when_parsed_then_local_prune() {
        assert_eq!(
            parse(&argv(&["worktree", "prune", "/repo"])).unwrap(),
            CliAction::PruneWorktrees {
                repo_path: "/repo".to_string()
            }
        );
    }

    #[test]
    fn given_worktree_bogus_subcommand_when_parsed_then_err() {
        assert!(parse(&argv(&["worktree", "bogus", "/repo"])).is_err());
    }

    // --- scheduler ----------------------------------------------------------

    #[test]
    fn given_scheduler_run_when_parsed_then_run_scheduler_once() {
        assert_eq!(
            parse(&argv(&["scheduler", "run", "7"])).unwrap(),
            CliAction::Request(Command::RunSchedulerOnce { project_id: 7 })
        );
    }

    #[test]
    fn given_scheduler_once_when_parsed_then_run_scheduler_once() {
        assert_eq!(
            parse(&argv(&["scheduler", "once", "7"])).unwrap(),
            CliAction::Request(Command::RunSchedulerOnce { project_id: 7 })
        );
    }

    #[test]
    fn given_scheduler_bogus_subcommand_when_parsed_then_err() {
        assert!(parse(&argv(&["scheduler", "bogus", "7"])).is_err());
    }

    // --- memory ------------------------------------------------------------

    #[test]
    fn given_memory_retrieve_when_parsed_then_request_memory_retrieve() {
        assert_eq!(
            parse(&argv(&["memory", "retrieve", "7", "merge", "policy"])).unwrap(),
            CliAction::Request(Command::MemoryRetrieve {
                project_id: 7,
                query: "merge policy".to_string(),
            })
        );
    }

    #[test]
    fn given_memory_save_text_when_parsed_then_request_memory_save() {
        assert_eq!(
            parse(&argv(&[
                "memory", "save", "7", "--kind", "result", "merged", "safely"
            ]))
            .unwrap(),
            CliAction::Request(Command::MemorySave {
                project_id: 7,
                kind: "result".to_string(),
                content: "merged safely".to_string(),
            })
        );
    }

    #[test]
    fn given_memory_save_without_content_when_parsed_then_err() {
        assert!(parse(&argv(&["memory", "save", "7", "--kind", "result"])).is_err());
    }

    #[test]
    fn given_memory_consolidate_when_parsed_then_request_memory_consolidate() {
        assert_eq!(
            parse(&argv(&[
                "memory",
                "consolidate",
                "7",
                "--mode",
                "deepsleep"
            ]))
            .unwrap(),
            CliAction::Request(Command::MemoryConsolidate {
                project_id: 7,
                mode: "deepsleep".to_string(),
            })
        );
    }

    #[test]
    fn given_memory_preset_ls_when_parsed_then_list_memory_presets() {
        assert_eq!(
            parse(&argv(&["memory", "preset", "ls"])).unwrap(),
            CliAction::Request(Command::ListMemoryPresets)
        );
    }

    #[test]
    fn given_memory_preset_set_when_parsed_then_upsert_memory_preset() {
        assert_eq!(
            parse(&argv(&[
                "memory",
                "preset",
                "set",
                "custom",
                "--retrieve-kind",
                "command",
                "--retrieve-cmd",
                "mem-get {query}",
                "--save-kind",
                "command",
                "--save-cmd",
                "mem-save {content_file}",
                "--dream-kind",
                "portable",
                "--deepsleep-kind",
                "portable"
            ]))
            .unwrap(),
            CliAction::Request(Command::UpsertMemoryPreset {
                name: "custom".to_string(),
                retrieve_kind: "command".to_string(),
                retrieve_cmd: Some("mem-get {query}".to_string()),
                save_kind: "command".to_string(),
                save_cmd: Some("mem-save {content_file}".to_string()),
                dream_kind: "portable".to_string(),
                dream_cmd: None,
                deepsleep_kind: "portable".to_string(),
                deepsleep_cmd: None,
            })
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

    #[tokio::test]
    async fn given_missing_socket_parent_when_start_lock_acquired_then_parent_created() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("missing").join("auwsx.sock");

        let lock = acquire_daemon_start_lock(&socket).await.unwrap();

        assert!(socket.parent().unwrap().is_dir());
        assert!(lock.path.exists());
    }

    // --- arsenal ------------------------------------------------------------

    #[test]
    fn given_arsenal_ls_when_parsed_then_list_arsenal_presets() {
        assert_eq!(
            parse(&argv(&["arsenal", "ls"])).unwrap(),
            CliAction::Request(Command::ListArsenalPresets)
        );
    }

    #[test]
    fn given_arsenal_list_when_parsed_then_list_arsenal_presets() {
        assert_eq!(
            parse(&argv(&["arsenal", "list"])).unwrap(),
            CliAction::Request(Command::ListArsenalPresets)
        );
    }

    #[test]
    fn given_arsenal_set_required_flags_when_parsed_then_upsert_without_review() {
        assert_eq!(
            parse(&argv(&[
                "arsenal", "set", "local", "--main", "mc", "--plan", "pc", "--work", "wc"
            ]))
            .unwrap(),
            CliAction::Request(Command::UpsertArsenalPreset {
                name: "local".to_string(),
                main_agent_cmd: "mc".to_string(),
                route_agent_cmd: "wc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: None,
            })
        );
    }

    #[test]
    fn given_arsenal_set_with_review_when_parsed_then_upsert_with_review() {
        assert_eq!(
            parse(&argv(&[
                "arsenal", "set", "local", "--main", "mc", "--plan", "pc", "--work", "wc",
                "--review", "rc"
            ]))
            .unwrap(),
            CliAction::Request(Command::UpsertArsenalPreset {
                name: "local".to_string(),
                main_agent_cmd: "mc".to_string(),
                route_agent_cmd: "wc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: Some("rc".to_string()),
            })
        );
    }

    #[test]
    fn given_arsenal_set_missing_name_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "arsenal", "set", "--main", "mc", "--plan", "pc", "--work", "wc"
        ]))
        .is_err());
    }

    #[test]
    fn given_arsenal_set_missing_main_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "arsenal", "set", "local", "--plan", "pc", "--work", "wc"
        ]))
        .is_err());
    }

    #[test]
    fn given_arsenal_set_missing_plan_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "arsenal", "set", "local", "--main", "mc", "--work", "wc"
        ]))
        .is_err());
    }

    #[test]
    fn given_arsenal_set_missing_work_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "arsenal", "set", "local", "--main", "mc", "--plan", "pc"
        ]))
        .is_err());
    }

    #[test]
    fn given_arsenal_set_extra_positional_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "arsenal", "set", "local", "extra", "--main", "mc", "--plan", "pc", "--work", "wc"
        ]))
        .is_err());
    }

    #[test]
    fn given_arsenal_unknown_subcommand_when_parsed_then_err() {
        assert!(parse(&argv(&["arsenal", "bogus"])).is_err());
    }

    // --- ask ---------------------------------------------------------------

    #[test]
    fn given_ask_default_when_parsed_then_recall_mode() {
        assert_eq!(
            parse(&argv(&["ask", "7", "what", "next"])).unwrap(),
            CliAction::Request(Command::AskProject {
                project_id: 7,
                mode: AskMode::Recall,
                question: "what next".to_string(),
            })
        );
    }

    #[test]
    fn given_ask_seek_mode_when_parsed_then_seek_mode() {
        assert_eq!(
            parse(&argv(&["ask", "7", "--mode", "seek", "what", "next"])).unwrap(),
            CliAction::Request(Command::AskProject {
                project_id: 7,
                mode: AskMode::Seek,
                question: "what next".to_string(),
            })
        );
    }

    #[test]
    fn given_ask_ls_when_parsed_then_list_answers() {
        assert_eq!(
            parse(&argv(&["ask", "ls", "7"])).unwrap(),
            CliAction::Request(Command::ListAskAnswers {
                project_id: 7,
                limit: 20,
            })
        );
    }

    #[test]
    fn given_ask_bogus_mode_when_parsed_then_err() {
        assert!(parse(&argv(&["ask", "7", "--mode", "bogus", "q"])).is_err());
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
                arsenal_preset_name: None,
                main_agent_cmd: "mc".to_string(),
                route_agent_cmd: "wc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: None,
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
                schedule_interval_min: None,
                schedule_cron: None,
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
                arsenal_preset_name: None,
                main_agent_cmd: "mc".to_string(),
                route_agent_cmd: "wc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: Some("rc".to_string()),
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
                schedule_interval_min: None,
                schedule_cron: None,
            })
        );
    }

    #[test]
    fn given_project_add_missing_required_flags_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "add", "demo", "/repo"])).is_err());
    }

    #[test]
    fn given_project_add_with_arsenal_when_parsed_then_commands_are_overrides() {
        assert_eq!(
            parse(&argv(&[
                "project",
                "add",
                "demo",
                "/repo",
                "--arsenal",
                "codex"
            ]))
            .unwrap(),
            CliAction::Request(Command::AddProject {
                name: "demo".to_string(),
                repo_path: "/repo".to_string(),
                default_branch: "main".to_string(),
                arsenal_preset_name: Some("codex".to_string()),
                main_agent_cmd: String::new(),
                route_agent_cmd: String::new(),
                plan_agent_cmd: String::new(),
                work_agent_cmd: String::new(),
                review_agent_cmd: None,
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
                schedule_interval_min: None,
                schedule_cron: None,
            })
        );
    }

    #[test]
    fn given_project_add_extra_positional_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project", "add", "demo", "/repo", "/script", "--main", "mc", "--plan", "pc", "--work",
            "wc"
        ]))
        .is_err());
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

    #[test]
    fn given_project_get_when_parsed_then_getproject() {
        assert_eq!(
            parse(&argv(&["project", "get", "7"])).unwrap(),
            CliAction::Request(Command::GetProject { project_id: 7 })
        );
    }

    #[test]
    fn given_project_merge_when_parsed_then_approve_project_merge() {
        assert_eq!(
            parse(&argv(&["project", "merge", "7"])).unwrap(),
            CliAction::Request(Command::ApproveProjectMerge { project_id: 7 })
        );
    }

    #[test]
    fn given_project_diagnose_when_parsed_then_diagnose_project() {
        assert_eq!(
            parse(&argv(&["project", "diagnose", "7"])).unwrap(),
            CliAction::Request(Command::DiagnoseProject { project_id: 7 })
        );
    }

    #[test]
    fn given_project_reconcile_when_parsed_then_reconcile_project_apply() {
        assert_eq!(
            parse(&argv(&["project", "reconcile", "7"])).unwrap(),
            CliAction::Request(Command::ReconcileProject {
                project_id: 7,
                dry_run: false
            })
        );
    }

    #[test]
    fn given_project_reconcile_dry_run_when_parsed_then_reconcile_project_dry_run() {
        assert_eq!(
            parse(&argv(&["project", "reconcile", "7", "--dry-run"])).unwrap(),
            CliAction::Request(Command::ReconcileProject {
                project_id: 7,
                dry_run: true
            })
        );
    }

    #[test]
    fn given_project_remote_get_when_parsed_then_get_remote_config() {
        assert_eq!(
            parse(&argv(&["project", "remote", "get", "7"])).unwrap(),
            CliAction::Request(Command::GetProjectRemoteConfig { project_id: 7 })
        );
    }

    #[test]
    fn given_project_remote_set_with_toggles_when_parsed_then_upsert_remote_config() {
        assert_eq!(
            parse(&argv(&[
                "project",
                "remote",
                "set",
                "7",
                "--url",
                "https://github.com/acme/repo",
                "--owner",
                "acme",
                "--repo",
                "repo",
                "--auth-kind",
                "none",
                "--inbound-auwsx-run",
                "--outbound-issue-create",
                "--remote-pr-merge",
                "--agent-comments",
                "--required-checks",
                "require_green",
                "--labels",
                "auwsx,agent",
                "--pr-base",
                "main"
            ]))
            .unwrap(),
            CliAction::Request(Command::UpsertProjectRemoteConfig {
                project_id: 7,
                provider: RemoteProvider::Github,
                remote_url: "https://github.com/acme/repo".to_string(),
                owner: "acme".to_string(),
                repo: "repo".to_string(),
                api_base_url: "https://api.github.com".to_string(),
                auth_kind: RemoteAuthKind::None,
                auth_ref: None,
                webhook_secret_ref: None,
                inbound_auwsx_run_enabled: true,
                outbound_issue_create_enabled: true,
                remote_pr_merge_enabled: true,
                agent_comment_sync_enabled: true,
                subtask_comment_sync_enabled: false,
                finding_comment_sync_enabled: false,
                draft_pr_enabled: false,
                required_checks_policy: RequiredChecksPolicy::RequireGreen,
                default_labels: Some("auwsx,agent".to_string()),
                default_assignees: None,
                pr_base_branch: Some("main".to_string()),
            })
        );
    }

    #[test]
    fn given_project_remote_set_bad_auth_kind_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project",
            "remote",
            "set",
            "7",
            "--url",
            "https://github.com/acme/repo",
            "--owner",
            "acme",
            "--repo",
            "repo",
            "--auth-kind",
            "pat"
        ]))
        .is_err());
    }

    #[test]
    fn given_project_remote_delete_when_parsed_then_delete_remote_config() {
        assert_eq!(
            parse(&argv(&["project", "remote", "delete", "7"])).unwrap(),
            CliAction::Request(Command::DeleteProjectRemoteConfig { project_id: 7 })
        );
    }

    #[test]
    fn given_project_remote_sync_runs_when_parsed_then_recent_remote_sync_runs() {
        assert_eq!(
            parse(&argv(&[
                "project",
                "remote",
                "sync-runs",
                "7",
                "--limit",
                "3"
            ]))
            .unwrap(),
            CliAction::Request(Command::RecentRemoteSyncRuns {
                project_id: 7,
                limit: 3,
            })
        );
    }

    #[test]
    fn given_project_remote_plan_when_parsed_then_plan_issue_remote_workflow() {
        assert_eq!(
            parse(&argv(&["project", "remote", "plan", "42"])).unwrap(),
            CliAction::Request(Command::PlanIssueRemoteWorkflow { issue_id: 42 })
        );
    }

    #[test]
    fn given_reconcile_apply_when_parsed_then_apply_reconcile() {
        assert_eq!(
            parse(&argv(&["reconcile", "apply", "11"])).unwrap(),
            CliAction::Request(Command::ApplyReconcile { main_job_id: 11 })
        );
    }

    #[test]
    fn given_reconcile_apply_extra_arg_when_parsed_then_err() {
        assert!(parse(&argv(&["reconcile", "apply", "11", "extra"])).is_err());
    }

    #[test]
    fn given_reconcile_apply_unexpected_flag_when_parsed_then_err() {
        assert!(parse(&argv(&["reconcile", "apply", "11", "--dry-run"])).is_err());
    }

    #[test]
    fn given_reconcile_apply_missing_id_when_parsed_then_err() {
        assert!(parse(&argv(&["reconcile", "apply"])).is_err());
    }

    #[test]
    fn given_project_diagnose_extra_arg_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "diagnose", "7", "extra"])).is_err());
    }

    #[test]
    fn given_project_diagnose_unexpected_flag_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "diagnose", "7", "--dry-run"])).is_err());
    }

    #[test]
    fn given_project_reconcile_extra_arg_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "reconcile", "7", "extra"])).is_err());
    }

    #[test]
    fn given_project_reconcile_unknown_flag_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "reconcile", "7", "--force"])).is_err());
    }

    #[test]
    fn given_project_reconcile_dry_run_value_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "reconcile", "7", "--dry-run=false"])).is_err());
    }

    #[test]
    fn given_project_get_non_integer_id_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "get", "notanint"])).is_err());
    }

    #[test]
    fn given_project_get_missing_id_when_parsed_then_err() {
        assert!(parse(&argv(&["project", "get"])).is_err());
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
    fn given_issue_add_when_parsed_then_err() {
        let err = parse(&argv(&["issue", "add", "2", "my", "title", "--desc", "d"]))
            .expect_err("manual issue add is not user-facing");
        assert!(err.to_string().contains("agent-derived"));
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
    fn given_issue_retry_when_parsed_then_retryissue() {
        assert_eq!(
            parse(&argv(&["issue", "retry", "5"])).unwrap(),
            CliAction::Request(Command::RetryIssue { issue_id: 5 })
        );
    }

    #[test]
    fn given_issue_merge_when_parsed_then_approve_issue_merge() {
        assert_eq!(
            parse(&argv(&["issue", "merge", "5"])).unwrap(),
            CliAction::Request(Command::ApproveIssueMerge { issue_id: 5 })
        );
    }

    #[test]
    fn given_issue_apply_merge_when_parsed_then_apply_issue_merge() {
        assert_eq!(
            parse(&argv(&["issue", "apply-merge", "5"])).unwrap(),
            CliAction::Request(Command::ApplyIssueMerge { issue_id: 5 })
        );
    }

    #[test]
    fn given_issue_absorb_when_parsed_then_absorbissue() {
        assert_eq!(
            parse(&argv(&["issue", "absorb", "1", "2"])).unwrap(),
            CliAction::Request(Command::AbsorbIssue {
                issue_id: 1,
                into_issue_id: 2,
            })
        );
    }

    #[test]
    fn given_issue_remove_when_parsed_then_removeissue() {
        assert_eq!(
            parse(&argv(&["issue", "remove", "5"])).unwrap(),
            CliAction::Request(Command::RemoveIssue { issue_id: 5 })
        );
    }

    #[test]
    fn given_issue_cleanup_when_parsed_then_cleanup_issue_worktree() {
        assert_eq!(
            parse(&argv(&["issue", "cleanup", "5"])).unwrap(),
            CliAction::Request(Command::CleanupIssueWorktree { issue_id: 5 })
        );
    }

    #[test]
    fn given_issue_status_invalid_status_when_parsed_then_err() {
        assert!(parse(&argv(&["issue", "status", "1", "bogus"])).is_err());
    }

    // --- subtask ------------------------------------------------------------

    #[test]
    fn given_subtask_add_when_parsed_then_err() {
        let _guard = CONTROL_OUTBOX_ENV_LOCK.lock().unwrap();
        std::env::remove_var(control_outbox::OUTBOX_ENV);
        let err = parse(&argv(&["subtask", "add", "1", "0", "do", "the", "thing"]))
            .expect_err("manual subtask add is not user-facing");
        assert!(err.to_string().contains("agent-derived"));
    }

    #[test]
    fn given_subtask_add_with_control_outbox_when_parsed_then_addsubtask() {
        let _guard = CONTROL_OUTBOX_ENV_LOCK.lock().unwrap();
        std::env::set_var(control_outbox::OUTBOX_ENV, "/tmp/auwsx-control.jsonl");
        let parsed = parse(&argv(&["subtask", "add", "1", "0", "do", "the", "thing"]));
        std::env::remove_var(control_outbox::OUTBOX_ENV);

        assert_eq!(
            parsed.unwrap(),
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
                "finding",
                "add",
                "1",
                "0",
                "blocker",
                "null",
                "deref",
                "--lens",
                "correctness",
                "--file",
                "src/x.rs"
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

    // --- queue messages -----------------------------------------------------

    #[test]
    fn given_queue_add_multiword_when_parsed_then_addsteering_human() {
        assert_eq!(
            parse(&argv(&["queue", "add", "1", "human", "please", "retry"])).unwrap(),
            CliAction::Request(Command::AddSteering {
                issue_id: 1,
                source: SteeringSource::Human,
                note: "please retry".to_string(),
            })
        );
    }

    #[test]
    fn given_queue_add_invalid_source_when_parsed_then_err() {
        assert!(parse(&argv(&["queue", "add", "1", "bogus", "note"])).is_err());
    }

    #[test]
    fn given_queue_consume_when_parsed_then_consumesteering() {
        assert_eq!(
            parse(&argv(&["queue", "consume", "1"])).unwrap(),
            CliAction::Request(Command::ConsumeSteering { issue_id: 1 })
        );
    }

    #[test]
    fn given_queue_ls_when_parsed_then_liststeering_pending_only() {
        assert_eq!(
            parse(&argv(&["queue", "ls", "1"])).unwrap(),
            CliAction::Request(Command::ListSteering {
                issue_id: 1,
                pending_only: true,
            })
        );
    }

    #[test]
    fn given_legacy_steering_alias_when_parsed_then_queue_command() {
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
                arsenal_preset_name: None,
                main_agent_cmd: "mc".to_string(),
                route_agent_cmd: "wc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: None,
                completion_policy: None,
                plan_gate_timeout_min: None,
                completion_soft_timeout_min: None,
                schedule_interval_min: None,
                schedule_cron: None,
            })
        );
    }

    #[test]
    fn given_schedule_shorthand_when_parsed_then_normalized_cron() {
        let CliAction::Request(Command::AddProject { schedule_cron, .. }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--schedule",
            "30m",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(schedule_cron, Some("*/30 * * * *".to_string()));
    }

    #[test]
    fn given_non_cron_representable_shorthand_when_parsed_then_every_repeat() {
        let CliAction::Request(Command::AddProject { schedule_cron, .. }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--schedule",
            "90m",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(schedule_cron, Some("@every 90m".to_string()));
    }

    #[test]
    fn given_schedule_cron_when_parsed_then_kept_as_cron() {
        let CliAction::Request(Command::AddProject { schedule_cron, .. }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--schedule",
            "15 9 * * 1-5",
        ]))
        .unwrap() else {
            panic!("expected AddProject")
        };
        assert_eq!(schedule_cron, Some("15 9 * * 1-5".to_string()));
    }

    #[test]
    fn given_schedule_min_when_parsed_then_legacy_interval_is_set() {
        let CliAction::Request(Command::AddProject {
            schedule_interval_min,
            schedule_cron,
            ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--schedule-min",
            "5",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(schedule_interval_min, Some(5));
        assert_eq!(schedule_cron, None);
    }

    #[test]
    fn given_invalid_schedule_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--schedule",
            "*/90 * * * *",
        ]))
        .is_err());
    }

    #[test]
    fn given_completion_policy_auto_when_parsed_then_policy_some_auto() {
        let CliAction::Request(Command::AddProject {
            completion_policy, ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-policy",
            "auto",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_policy, Some(CompletionPolicy::Auto));
    }

    #[test]
    fn given_completion_policy_soft_when_parsed_then_policy_some_soft() {
        let CliAction::Request(Command::AddProject {
            completion_policy, ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-policy",
            "soft",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_policy, Some(CompletionPolicy::Soft));
    }

    #[test]
    fn given_completion_policy_bogus_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-policy",
            "bogus",
        ]))
        .is_err());
    }

    #[test]
    fn given_completion_policy_uppercase_when_parsed_then_err() {
        // from_str is exact lowercase; `AUTO` is not a known variant.
        assert!(parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-policy",
            "AUTO",
        ]))
        .is_err());
    }

    #[test]
    fn given_completion_policy_empty_value_via_equals_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-policy=",
        ]))
        .is_err());
    }

    #[test]
    fn given_completion_policy_equals_form_when_parsed_then_some_auto() {
        let CliAction::Request(Command::AddProject {
            completion_policy, ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-policy=auto",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_policy, Some(CompletionPolicy::Auto));
    }

    #[test]
    fn given_plan_gate_timeout_zero_when_parsed_then_some_zero() {
        let CliAction::Request(Command::AddProject {
            plan_gate_timeout_min,
            ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--plan-gate-timeout",
            "0",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(plan_gate_timeout_min, Some(0));
    }

    #[test]
    fn given_plan_gate_timeout_fifteen_when_parsed_then_some_fifteen() {
        let CliAction::Request(Command::AddProject {
            plan_gate_timeout_min,
            ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--plan-gate-timeout",
            "15",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(plan_gate_timeout_min, Some(15));
    }

    #[test]
    fn given_plan_gate_timeout_non_integer_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--plan-gate-timeout",
            "notanint",
        ]))
        .is_err());
    }

    #[test]
    fn given_negative_plan_gate_timeout_when_parsed_then_some_negative() {
        // `-5` is taken as the flag value (not another flag) and i64 accepts it;
        // no sign validation at the parse layer.
        let CliAction::Request(Command::AddProject {
            plan_gate_timeout_min,
            ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--plan-gate-timeout",
            "-5",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(plan_gate_timeout_min, Some(-5));
    }

    #[test]
    fn given_bare_plan_gate_timeout_flag_at_end_when_parsed_then_none() {
        // A `--key` with no following value is classified as a boolean flag, so
        // the optional int reads as absent (None), not an error. ^ pins this.
        let CliAction::Request(Command::AddProject {
            plan_gate_timeout_min,
            ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--plan-gate-timeout",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(plan_gate_timeout_min, None);
    }

    #[test]
    fn given_completion_timeout_thirty_when_parsed_then_some_thirty() {
        let CliAction::Request(Command::AddProject {
            completion_soft_timeout_min,
            ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-timeout",
            "30",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_soft_timeout_min, Some(30));
    }

    #[test]
    fn given_merge_delay_thirty_when_parsed_then_completion_soft_timeout_some_thirty() {
        let CliAction::Request(Command::AddProject {
            completion_soft_timeout_min,
            ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--merge-delay",
            "30",
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
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-timeout",
            "abc",
        ]))
        .is_err());
    }

    #[test]
    fn given_large_completion_timeout_when_parsed_then_some_i64_max() {
        let CliAction::Request(Command::AddProject {
            completion_soft_timeout_min,
            ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-timeout",
            "9223372036854775807",
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
                "project",
                "add",
                "demo",
                "/repo",
                "--main",
                "mc",
                "--plan",
                "pc",
                "--work",
                "wc",
                "--completion-policy",
                "soft",
                "--plan-gate-timeout",
                "0",
                "--completion-timeout",
                "45"
            ]))
            .unwrap(),
            CliAction::Request(Command::AddProject {
                name: "demo".to_string(),
                repo_path: "/repo".to_string(),
                default_branch: "main".to_string(),
                arsenal_preset_name: None,
                main_agent_cmd: "mc".to_string(),
                route_agent_cmd: "wc".to_string(),
                plan_agent_cmd: "pc".to_string(),
                work_agent_cmd: "wc".to_string(),
                review_agent_cmd: None,
                completion_policy: Some(CompletionPolicy::Soft),
                plan_gate_timeout_min: Some(0),
                completion_soft_timeout_min: Some(45),
                schedule_interval_min: None,
                schedule_cron: None,
            })
        );
    }

    #[test]
    fn given_completion_timeout_overflow_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-timeout",
            "9223372036854775808",
        ]))
        .is_err());
    }

    #[test]
    fn given_plan_gate_timeout_overflow_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--plan-gate-timeout",
            "99999999999999999999",
        ]))
        .is_err());
    }

    #[test]
    fn given_completion_policy_repeated_when_parsed_then_last_wins_soft() {
        let CliAction::Request(Command::AddProject {
            completion_policy, ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-policy",
            "auto",
            "--completion-policy",
            "soft",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(completion_policy, Some(CompletionPolicy::Soft));
    }

    #[test]
    fn given_completion_policy_whitespace_value_when_parsed_then_err() {
        assert!(parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-policy",
            " ",
        ]))
        .is_err());
    }

    #[test]
    fn given_plan_gate_timeout_equals_form_zero_when_parsed_then_some_zero() {
        let CliAction::Request(Command::AddProject {
            plan_gate_timeout_min,
            ..
        }) = parse(&argv(&[
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--plan-gate-timeout=0",
        ]))
        .unwrap()
        else {
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
            "project",
            "add",
            "demo",
            "/repo",
            "--main",
            "mc",
            "--plan",
            "pc",
            "--work",
            "wc",
            "--completion-policy",
            "auto",
        ]))
        .unwrap()
        else {
            panic!("expected AddProject")
        };
        assert_eq!(
            (plan_gate_timeout_min, completion_soft_timeout_min),
            (None, None)
        );
    }

    #[test]
    fn given_control_chars_when_printable_multiline_then_strips_except_tab_and_newline() {
        assert_eq!(
            printable_multiline("ok\u{1b}[31m\tline\nnext\u{7}"),
            "ok[31m\tline\nnext"
        );
    }
}

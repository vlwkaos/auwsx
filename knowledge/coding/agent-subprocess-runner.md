---
slug: agent-subprocess-runner
kind: coding
title: auwsx agent subprocess runner (direct tokio::process, no tmux)
description: The agent runner — a direct tokio::process child auwsx owns (no tmux, no trait), with build_argv {prompt}-or-stdin substitution, timeout kill+reap, ExitKind classification, spawn-fail-as-Error-outcome, and per-agent DEFAULT_CMD templates.
keywords: [agent run AgentSpec AgentOutcome, tokio process child, no tmux no trait, build_argv prompt substitution, prompt to stdin opencode, no shell no injection, timeout start_kill reap kill_on_drop, ExitKind Exited Killed Timeout Error, spawn failure Ok Error outcome, DEFAULT_CMD claude codex opencode, ask mode main agent, combined stdout stderr log_path, agent subprocess runner]
created: 2026-06-09
modified: 2026-06-17
---

# auwsx agent subprocess runner

`crates/auwsx-core/src/agent/mod.rs` + `{claude,codex,opencode}.rs`.

## No tmux, no trait

`agent::run(AgentSpec) -> Result<AgentOutcome>` spawns the agent CLI as a
**direct `tokio::process` child auwsx owns**. The OLD `AgentRunner` trait + 3
impls + `AgentHandle`/`SignalDone`/tmux are GONE (replaced in the 2026-06-09
issue-model pivot). tmux/wsx shell is now optional, human-only (spectating).

## argv construction (no shell)

`build_argv(template, prompt)` splits the template on whitespace:
- Tokens containing `{prompt}` get the substring replaced — prompt stays **ONE
  arg** even with spaces.
- If **no** `{prompt}` token → prompt is fed to child **stdin** (the `opencode`
  shape).

So prompt content can never inject args. Tradeoff: templates can't use
pipes/quotes — plain flag list + `{prompt}`.

## Capture + timeout

- **Capture**: combined stdout+stderr → `log_path` (truncate).
- **Timeout**: `tokio::time::timeout` on `child.wait()`; on expiry `start_kill()`
  + reap → `ExitKind::Timeout`. `kill_on_drop(true)`.

## ExitKind classification

| Situation | Result |
|-----------|--------|
| exit code present | `Exited` |
| signaled (code None) | `Killed` |
| timeout | `Timeout` |
| spawn failure (bad binary) | returns `Ok(AgentOutcome{Error,..})` (NOT `Err`) + writes failure note to log |
| log-file setup I/O failure | propagates as `Err` |

Only log-file setup I/O failures propagate as `Err`; everything else is a
classified outcome.

## DEFAULT_CMD templates

`{claude,codex,opencode}.rs` are reduced to `NAME` + `DEFAULT_CMD` template
constants. claude/codex use `{prompt}`; opencode uses stdin (no `{prompt}`
token). agent_runs records role/phase/exit_kind/log_path/timestamps per run.

One-shot project ask mode reuses the same subprocess execution path and the
project's configured main-agent command. It is not an issue phase, so it stores
answers in `ask_answers` rather than `agent_runs`, while still keeping a log path
for operator inspection.

## Test notes

Runner tests use REAL `sh`/`echo`/`cat`/`sleep`/`pwd` + temp scripts —
deterministic, no mocking. The `Killed`-by-signal (non-timeout) path is the one
documented untested branch.

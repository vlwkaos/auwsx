---
slug: daemon-spine
kind: history
title: daemon spine shipped — typed CRUD, IPC daemon, CLI, agent runner
description: Record of the daemon backbone that shipped after the 2026-06-09 issue-model pivot — typed DB CRUD, Unix-socket IPC daemon + client, the auwsx CLI, and the direct-subprocess agent runner — to a runnable, 383-test product.
keywords: [daemon spine shipped, typed db crud, ipc daemon Command Response Event, EventStream, auwsx CLI hand-rolled parser, subprocess agent runner, agent_runs log, 383 tests green, verified live end-to-end, issue model pivot, commits 4f02fed 3035925, scheduler pipeline still pending]
created: 2026-06-09
modified: 2026-06-09
---

# daemon spine (shipped 2026-06-09)

The spine designed in the 2026-06-09 issue-model ledger landed on `main`.
**Runnable product, 383 tests green**, verified end-to-end live (daemon ↔ socket
↔ CLI persists to SQLite). Built in dependency order: CRUD → IPC → CLI → runner.

## What landed

| Layer | Module(s) | Surface |
|-------|-----------|---------|
| Typed DB CRUD | `db/{projects,issues,subtasks,findings,agent_runs}.rs`, CRUD in `backlog.rs`/`steering.rs` | row structs + hand-rolled `as_str`/`from_str` + async CRUD (see coding/db-crud-conventions.md) |
| IPC | `ipc.rs` | `Command`/`Response`/`Event`, `serve`/`request`/`EventStream`, pure `dispatch` (see coding/ipc-protocol.md) |
| CLI | `auwsx-tui/src/cli.rs` + `main.rs` | `auwsx daemon` + thin IPC-client subcommands; pure hand-rolled `parse` (no clap) |
| Agent runner | `agent/mod.rs` + `{claude,codex,opencode}.rs` | `run(AgentSpec)`, `DEFAULT_CMD` templates (see coding/agent-subprocess-runner.md) |

Earlier, the 2026-06-09 redesign reimplemented the schema (`0001_init.sql`, 10
tables) + `state.rs` (`IssueStatus` + scheduler classes + 37-transition matrix),
replacing the OLD task/tmux model.

## Key commits (on `main`)

| Commit | Content |
|--------|---------|
| `e0a3014` | replace task pipeline with issue model (schema + state machine) |
| `4f02fed` | typed DB CRUD + IPC daemon/client + auwsx CLI |
| `3035925` | direct-subprocess agent runner + agent_runs log |
| `d7815b8` | (follow-up in the spine series) |
| `6ba8dad` | (earlier) state machine + db layer |

## Tests

All via `/write-test` (blind writer + adversarial reviewer; never inline).
`tests/crud.rs` (164), `tests/ipc.rs` (65), `tests/agent.rs` (41),
`cli.rs` `#[cfg(test)]` (48, in-file since `auwsx-tui` is bin-only),
`state` (46), `db_smoke` (19) = **383**.

## Autonomy loop (shipped 2026-06-09, commit `ced3c47`)

On top of the spine, the daemon now drives issues autonomously: a scheduler
ticker + pipeline phase fns + worktree lifecycle, all behind ports & adapters
(Clock / AgentExecutor / Worktrees) so the drive loop is deterministically
testable. Design + seams in coding/scheduler-pipeline.md.

**Verified LIVE**: the real daemon drove an issue CONSOLIDATING→PLANNING→PLANNED
via real agent subprocesses that called back over the socket
(`auwsx issue status …`), created a real git worktree at PLANNING, then parked
correctly at the PLANNED soft gate. **416 tests green** (33 new in
`tests/autonomy.rs`).

## Not yet built (handed to plan.md)

main_jobs queue + routines, inbox watcher, config load/save, per-run agent token
scoping, launchd, TUI v0.1. (Exposing completion_policy + gate timeouts on
`project add` — needed to close the live loop to DONE without raw SQL — was
since built in `ed72491`; see history/project-config-cli-flags.md.)

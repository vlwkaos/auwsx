---
slug: ipc-protocol
kind: coding
title: auwsx IPC protocol (Unix-socket Command/Response/Event)
description: The JSON-lines Unix-socket IPC between daemon and clients — the serde internal-vs-adjacent tagging gotcha and why, serve/request/EventStream transport, socket path resolution, remote repo config commands, and pure dispatch.
keywords: [ipc.rs, Command Response Event, project remote config, GetProjectRemoteConfig, UpsertProjectRemoteConfig, RecentRemoteSyncRuns, serde internal tagging tag kind, serde adjacent tagging content data, cannot serialize tagged newtype variant, JSON lines UnixListener, serve request EventStream, AskProject ListAskAnswers AskAnswered, socket path AUWSX_SOCK XDG_RUNTIME_DIR, pure dispatch unit-testable, Notify shutdown SIGINT, agent token TUI caller scoping, ipc protocol]
created: 2026-06-09
modified: 2026-07-01
---

# auwsx IPC protocol

`crates/auwsx-core/src/ipc.rs`. JSON-lines over a Unix socket. One command
surface, two clients: the agent control CLI (`auwsx issue ...`) and the human
TUI hit the SAME daemon ops. Agent ops ⊆ human ops; IPC enforces per-caller
(agent token vs TUI). Human override is first-class at every gate.

Global config commands include `ListArsenalPresets` and
`UpsertArsenalPreset`; Arsenal presets are reusable per-role agent command
templates consumed by project forms and stored separately from project rows.

Project remote repository config is also exposed through pure dispatch:
`GetProjectRemoteConfig`, `UpsertProjectRemoteConfig`,
`DeleteProjectRemoteConfig`, `RecentRemoteSyncRuns`, and
`PlanIssueRemoteWorkflow`. Config commands only read/write typed config, link,
event, and sync-run rows. `PlanIssueRemoteWorkflow` is read-only and returns
the pure `remote_plan::RemoteWorkflowPlan` for one local issue. Provider network
I/O belongs in daemon runtime services layered on top of the same model. CLI/TUI
clients must use these IPC commands instead of opening SQLite.

Runtime-owned manual commands include `RunSchedulerOnce`, `RunIssueNow`,
`RunBacklogNow`, `RunRoutineNow`, `RemoveIssue`, and `AskProject`. They require
the daemon's `Scheduler` instance, not pure `dispatch`, because they touch
running-agent guards, worktree cleanup, runtime queues, or subprocess execution.
`AskProject` snapshots project status, issues, live backlog, and routines, asks
the configured main agent once in recall/seek mode, stores the answer, and emits
`Event::AskAnswered`. `ListAskAnswers` is pure dispatch: it reads the persisted
project-level answer stack.

## Tagging gotcha (cost a debug cycle)

An enum used as a JSON-lines wire reply with **newtype variants wrapping a
primitive / sequence / option** CANNOT use serde **internal** tagging
(`#[serde(tag="kind")]`):

> `serde_json` errors "cannot serialize tagged newtype variant … containing an
> integer/sequence".

It **silently works for unit + struct variants**, so in-process dispatch tests
pass and only the socket path fails — masking the bug.

**Fix: adjacent tagging** `#[serde(tag="kind", content="data")]` — payload goes
in its own `data` field; every variant shape round-trips.

| Type | Variants | Tagging |
|------|----------|---------|
| `Response` | has `Id(i64)`, `Projects(Vec<_>)`, `Project(Option<_>)` | **adjacent** (required) |
| `Command` | all unit/struct variants | internal (fine) |

The `Response` type carries a doc comment as the discoverable hint.

## Transport

- `serve(db, events, &socket, shutdown: Arc<Notify>)` — JSON-lines over
  `UnixListener`, per-conn task. Removes stale socket on bind + on exit.
  `Shutdown` command or SIGINT → `Notify`.
- `request(&socket, &cmd)` — one-shot request/response.
- `EventStream::connect` — drains the Ok ack, then yields `Event`s.
- **Socket path resolution**: `$AUWSX_SOCK` > `$XDG_RUNTIME_DIR/auwsx.sock` >
  cache dir > `$TMPDIR`.

## Pure dispatch

`dispatch(db, events, now, cmd) -> Response` is **pure** → unit-tested without a
socket. Transport is tested separately. The clock (`now`) is injected, matching
the CRUD clock-injection convention.

## Open (unmechanized as of 2026-06-09)

Agent-vs-TUI caller scoping over the socket is specified but not yet mechanized.
The agent callback env (`AUWSX_SOCK`/`AUWSX_ISSUE_ID`/token injection) is part of
the pending pipeline-phases work.

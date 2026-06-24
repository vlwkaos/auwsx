---
slug: scheduler-pipeline
kind: coding
title: autonomy loop — scheduler + pipeline via ports & adapters
description: How the daemon drives an issue through phases using pure plan_phase/decide domain logic, pipeline::execute, Scheduler runtime, issue-scoped callbacks, and deterministic drive-loop tests.
keywords: [scheduler, pipeline, ports and adapters, hexagonal, Clock SystemClock, AgentExecutor SubprocessExecutor, Worktrees WsxWorktrees, plan_phase, scheduler decide, Spawn SoftGate Teardown, soft_gate_due, project_due_for_auto_tick, schedule_interval_min, AUWSX_TICK_SECS, tick_project, spawn_phase running set, pipeline execute AgentSpec, agent callback AUWSX_SOCK AUWSX_ISSUE_ID, issue-local proxy allowlist, daemon wiring stop Notify, ask_project, drive loop test FixedClock ScriptedAgent, max_concurrency, spawn_blocking wsx-core, autonomy loop how does the daemon drive an issue]
created: 2026-06-09
modified: 2026-06-24
---

# autonomy loop: scheduler + pipeline

The daemon drives issues autonomously on top of the spine
(history/daemon-spine.md). Domain logic is pure + testable; all infrastructure
sits behind traits so the whole drive loop runs deterministically with fakes
(no real process / git / clock). Shipped + live-verified — see
history/daemon-spine.md.

## Ports & adapters (hexagonal)

| Port (trait) | Prod adapter | Why isolated |
|--------------|--------------|--------------|
| `clock::Clock` | `SystemClock` | scheduler decisions deterministic under a fixed clock |
| `agent::AgentExecutor` | `SubprocessExecutor` (→`agent::run`) | THE drive-loop seam: a fake reads `AUWSX_ISSUE_ID` from `spec.env` and applies the transition a real agent would make via the CLI — no real process needed |
| `worktree::Worktrees` | `WsxWorktrees` | wsx_core ops are SYNC (shell out to git) → wrap in `tokio::task::spawn_blocking`; test with a temp-dir fake |

## Pure domain (no IO — test directly)

- `pipeline::plan_phase(status) -> Option<(Role, needs_worktree)>`. The
  `Some`-set == `is_actionable`. Map:
  New/Planning→(Plan, true);
  Working/Fixing/Auditing/ResolvingConflict/Merging→(Work, true);
  Reviewing→(Review, true).
- `scheduler::decide(issues, project, running, now) -> Vec<Decision>` where
  `Decision = Spawn | SoftGate | Teardown`:
  - caps spawns at `max_concurrency - running.len()`;
  - skips issues already in `running`;
  - emits `SoftGate` only when due — `soft_gate_due(issue, project, now)` =
    soft-releasable AND (`wait_until` is None OR `now >= wait_until`);
  - emits `Teardown` only for Done-with-worktree.
- `prompt::build(&PromptContext) -> Option<String>` — per-phase prompt; `None`
  iff non-actionable.

## Effectful: pipeline::execute(&Deps, issue_id)

load issue → `plan_phase` → ensure worktree (only if `needs_worktree` &&
`worktree_path` is None; persist branch + path) → deterministic phase entry
(`NEW` becomes `PLANNING` before prompt/snapshot/run) → `cwd` = worktree else
`repo_path` → load steering / subtasks / findings → `prompt::build` →
`agent_runs::start` → `executor.execute(AgentSpec)` → replay the issue-local
control outbox → reload status for `status_after` → `agent_runs::finish`. Run
logs + prompts go under `AUWSX_DATA_DIR` at `<data>/runs/issue-N/` — NOT in the
repo (the agent writes its own `.auwsx/` artifacts in the worktree).

## Scheduler runtime

`Scheduler` is `Clone`; holds `Arc<dyn ports>` + `Db` + bus + socket +
`running: Arc<Mutex<HashSet>>` + `inflight: Vec<JoinHandle>`.

- `tick_project` — `decide` + dispatch each `Decision`.
- `spawn_phase` — reserves the slot in `running`, spawns a task that
  **self-removes** from `running` on completion.
- `service_soft_gate` — arms `wait_until` (= `now + timeout_min*60_000`) or
  releases (applies the transition) when `deadline <= 0` (auto policy) or due.
- `teardown` — removes worktree + clears `branch`/`worktree_path`; scheduled
  automatically for `DONE` and `ABANDONED`, not `FAILED`.
- `cleanup_issue_worktree_by_id` / `auwsx issue cleanup <issue_id>` — explicit
  operator cleanup for preserved failed worktrees.
- `remove_project(shallow=false)` — refuses running issues, tears down every
  DB-recorded issue worktree, then prunes any remaining git-registered
  `auwsx/issue-*` worktrees for the repo before removing the project row.
- `auwsx worktree prune <repo_path>` — local recovery command for orphaned
  managed issue worktrees after an out-of-band DB reset.
- `recover_interrupted_work` — daemon-start restoration: closes open issue and
  main-job `agent_runs`, marks interrupted issue/main-job work `FAILED`, and
  leaves queued main jobs queued.
- `join_inflight` (drain, used by tests) / `prune_inflight` (drop finished
  handles so the Vec doesn't grow).
- `run(shutdown)` — ticks all projects every `tick_interval`.

## Project auto-tick cadence

`projects.schedule_interval_min` controls whether the daemon evaluates a project
on the global loop:

| Value | Meaning |
|-------|---------|
| NULL | manual only; no auto-tick, only explicit scheduler IPC commands |
| `<= 0` | due on every global daemon loop |
| `n > 0` | due when `now - last_auto_tick >= n * 60_000` |

The global loop cadence is `AUWSX_TICK_SECS` with default 10 seconds and a
minimum of 1 second. A due tick only evaluates scheduler decisions; it spawns an
agent only when work is actionable and `max_concurrency` has capacity.

`Scheduler::ask_project` is a runtime-owned side path: it gathers project
context, executes the configured main agent once, persists `ask_answers`, and
emits an answer event. It does not mutate issue status or scheduler cadence.

## Agent callback contract

Agent reports back by running the SAME `auwsx` CLI as a thin IPC client. The
pipeline injects into the child env: `AUWSX_SOCK`, `AUWSX_ISSUE_ID`,
`AUWSX_AGENT_ROLE`. Prompts tell it to run
`auwsx issue status "$AUWSX_ISSUE_ID" <NEXT>`.

Security target from the 2026-06-24 backpressure audit: issue-local socket/proxy
access must be filtered by the same issue-scoped allowlist as the control
outbox, or issue workers should not receive `AUWSX_SOCK`. A regression should
prove issue-local control cannot call global settings mutations such as
`UpdateGlobalSettings`.

## Daemon wiring

`run_daemon` runs the IPC server + `Scheduler::run` concurrently over one `Db` +
bus. `ipc::serve` keeps its own `Arc<Notify>` shutdown (so the 65 ipc tests
stand); the scheduler gets its OWN stop `Notify`, fired AFTER `serve` returns
(SIGINT or `daemon stop`), then its task is awaited. Tick cadence is
`AUWSX_TICK_SECS` (default 10) — short so the loop reacts quickly when an agent
advances an issue.

## Drive-loop test pattern

Deterministic end-to-end without real IO:
`FixedClock` + `FakeWorktrees` (temp dir, no git) + `ScriptedAgent` (impl
`AgentExecutor`: reads `AUWSX_ISSUE_ID` from env, looks up current status,
applies the next transition via `issues::transition`, returns `Exited/0`).
Set the project `completion_policy = 'auto'` + `plan_gate_timeout_min = 0` (raw
sqlx UPDATE) so soft gates release with NO time travel. Drive by hand: loop
`tick_project` + `join_inflight` until Done. Assert Done reached, worktree torn
down, every `agent_runs` row finished with `status_after` + `exit_kind=Exited`.
Note: `NEW→PLANNING` is pipeline-owned phase entry. `PLAN_READY→WORKING` and
`READY_TO_MERGE→MERGING` are scheduler soft-gate releases, NOT agent
transitions. `READY_TO_MERGE→WORKING` is the human loop-back path for extra
verification, queue-message work, or branch polishing before merge.

Prompt review surface: the TUI Settings screen (`S`) renders the Prompt Catalog
from `auwsx_core::prompt::preview_catalog()`. Use that for reviewing all phase
prompts at once; live run artifacts remain the source for issue-specific prompt
context.

## Gotchas

- `decide(now)` with `now` unused triggers clippy unused-var. Moved due-ness
  INTO `decide` via `soft_gate_due(issue, project, now)` so `SoftGate` is only
  emitted when arming is needed or the gate expired — `now` is now meaningful
  and there is less churn.
- Daemon shutdown: sharing ONE `Notify` between `serve` and the scheduler only
  wakes one (`notify_one`). Give the scheduler its OWN stop `Notify` fired after
  serve returns; keep serve's signature so the ipc tests stand.
  (CancellationToken would wake all but would churn the 65 ipc tests.)
- wsx_core worktree ops are synchronous (shell out to git) → wrap each in
  `tokio::task::spawn_blocking` inside the async `Worktrees` adapter.
- Demo `rm -rf` cleanup is blocked by the sandbox on `.git` files → needs
  `dangerouslyDisableSandbox`.

---
slug: scheduler-pipeline
kind: coding
title: autonomy loop — scheduler + pipeline via ports & adapters
description: How the daemon drives an issue through phases autonomously — pure plan_phase/decide domain, effectful pipeline::execute + Scheduler runtime, agent callback over the socket, all infra isolated behind Clock/AgentExecutor/Worktrees ports for deterministic drive-loop tests.
keywords: [scheduler, pipeline, ports and adapters, hexagonal, Clock SystemClock, AgentExecutor SubprocessExecutor, Worktrees WsxWorktrees, plan_phase, scheduler decide, Spawn SoftGate Teardown, soft_gate_due, tick_project, spawn_phase running set, pipeline execute AgentSpec, agent callback AUWSX_SOCK AUWSX_ISSUE_ID, daemon wiring stop Notify, drive loop test FixedClock ScriptedAgent, max_concurrency, spawn_blocking wsx-core, autonomy loop how does the daemon drive an issue]
created: 2026-06-09
modified: 2026-06-09
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
  Consolidating→(Main, false); Planning→(Plan, true);
  Implementing/NeedsFix/Audit/Conflicted/Completing→(Work, true);
  Review→(Review, true).
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
`worktree_path` is None; persist branch + path) → `cwd` = worktree else
`repo_path` → load steering / subtasks / findings → `prompt::build` →
`agent_runs::start` → `executor.execute(AgentSpec)` → reload status for
`status_after` → `agent_runs::finish`. Run logs + prompts go under
`AUWSX_DATA_DIR` at `<data>/runs/issue-N/` — NOT in the repo (the agent writes
its own `.auwsx/` artifacts in the worktree).

## Scheduler runtime

`Scheduler` is `Clone`; holds `Arc<dyn ports>` + `Db` + bus + socket +
`running: Arc<Mutex<HashSet>>` + `inflight: Vec<JoinHandle>`.

- `tick_project` — `decide` + dispatch each `Decision`.
- `spawn_phase` — reserves the slot in `running`, spawns a task that
  **self-removes** from `running` on completion.
- `service_soft_gate` — arms `wait_until` (= `now + timeout_min*60_000`) or
  releases (applies the transition) when `deadline <= 0` (auto policy) or due.
- `teardown` — removes worktree + clears `branch`/`worktree_path`.
- `join_inflight` (drain, used by tests) / `prune_inflight` (drop finished
  handles so the Vec doesn't grow).
- `run(shutdown)` — ticks all projects every `tick_interval`.

## Agent callback contract

Agent reports back by running the SAME `auwsx` CLI as a thin IPC client. The
pipeline injects into the child env: `AUWSX_SOCK`, `AUWSX_ISSUE_ID`,
`AUWSX_AGENT_ROLE`. Prompts tell it to run
`auwsx issue status "$AUWSX_ISSUE_ID" <NEXT>`. No per-run token / caller scoping
yet — the local 0700 socket is the v1 boundary (scoping deferred, see plan.md).

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
Note: PLANNED→IMPLEMENTING and ENDED→COMPLETING are scheduler soft-gate releases,
NOT agent transitions.

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

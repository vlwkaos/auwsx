# auwsx

Autonomous workspace orchestrator. Sibling to `wsx`; depends on `wsx-core` library crate.

## Session Start

Read the design plan at `~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md` first. Architecture, state machine, CRUD matrix, and verification flow all live there.

## Quick Reference

```bash
cargo build --workspace             # NOT before /sec on dep set
cargo fmt -p auwsx-core -p auwsx-tui -p auwsx-web
cargo run --bin auwsx               # default = TUI (auto-starts daemon if none running)
cargo run --bin auwsx -- daemon     # start daemon explicitly
```

## Command Notes

- Formatting this worktree only: `cargo fmt --package auwsx-tui`. `cargo fmt --all` also targets sibling `wsx` crates outside this worktree.
- If `$AUWSX_BIN subtask ...` points at an older sibling build without `subtask`, run `cargo build -p auwsx-tui` and use `target/debug/auwsx subtask done <id>`.
- For review findings, use the literal provided id: `"$AUWSX_BIN" finding accept 3 "..."`; there is no `$AUWSX_FINDING_ID` env var in issue workers.

## Key Files (planned, mostly stubs at scaffold time)

- `crates/auwsx-core/src/state.rs` — `IssueStatus` enum + scheduler classes + transition matrix
- `crates/auwsx-core/src/pipeline.rs` — pure `plan_phase` (status→role) + `execute` (worktree→prompt→spawn→record); ports in `Deps`
- `crates/auwsx-core/src/scheduler.rs` — pure `decide` (issues→`Decision`s) + `Scheduler` runtime (per-tick dispatch, running-set, soft-gate, teardown)
- `crates/auwsx-core/src/{clock,worktree,prompt}.rs` — ports/adapters: `Clock`+`SystemClock`; `Worktrees`+`WsxWorktrees` (wsx-core); per-phase prompt builder
- `crates/auwsx-core/src/agent/mod.rs` also defines the `AgentExecutor` port + `SubprocessExecutor` (the test seam for the drive loop)
- `crates/auwsx-core/src/backlog.rs` — backlog_items CRUD (source/approval) + triage/consolidation
- `crates/auwsx-core/src/steering.rs` — append-only steering into in-flight issues
- `crates/auwsx-core/src/main_jobs.rs` — main-workspace lifecycle, queued ops
- `crates/auwsx-core/src/routines.rs` — cron routines (incl. built-ins: triage, deepsleep, dream, morning-summary)
- `crates/auwsx-core/src/inbox.rs` — `notify` watcher on `~/.auwsx/inbox/*.txt`
- `crates/auwsx-core/src/agent/mod.rs` — direct-subprocess runner (`run`/`AgentSpec`/`AgentOutcome`/`ExitKind`, `{prompt}`-or-stdin); `{claude,codex,opencode}.rs` hold per-agent `DEFAULT_CMD` templates
- `crates/auwsx-core/src/ipc.rs` — Unix-socket `Command`/`Response`/`Event` protocol + `serve`/`request`/`EventStream` + unit-testable `dispatch`
- `crates/auwsx-core/src/db/{projects,issues,subtasks,findings,agent_runs}.rs` — typed row structs + CRUD (issues/backlog/steering/findings persistence + append-only agent run log; `db/mod.rs` re-exports the row types)
- `crates/auwsx-core/src/db/migrations/0001_init.sql` — full schema
- `crates/auwsx-tui/src/cli.rs` — `auwsx` CLI: pure `parse` (arg grammar) + `run_daemon`/`run_request` IPC client glue
- `crates/auwsx-tui/src/app.rs` — ratatui top-level state + view router
- `crates/auwsx-tui/src/input.rs` — keybind table

## Conventions

- Pin deps to exact patch (`=X.Y.Z`); new package requires `/sec` first.
- Tests via `/write-test`, never inline.
- TUI colors: reference `ui::theme` roles only — NO inline `ratatui::style::Color::X` anywhere under `crates/auwsx-tui/src/ui/` (except `theme.rs` itself). Add new semantic roles to `theme.rs`; keep `BORDER` distinct from `TEXT_DIM`/`HINT` so chrome never collides with content.
- Skills bundled in `skills/`; copied to `~/.claude/skills/` on first run only if missing (never overwrite user copies).
- All filesystem state outside the repo: `~/.local/share/auwsx/state.db`, `~/.auwsx/inbox/`, `<task-worktree>/.auwsx/`.
- No AI attribution in commits or PRs.

- Uncertain about project term/schema/convention/prior decision → `/seek <topic>` first (lightweight KB lookup; same tier as grep/Glob).

## Command Notes

- Finding adjudication needs the literal finding id when `$AUWSX_FINDING_ID` is unset: `"$AUWSX_BIN" finding accept 2 "<how you'll fix>"`

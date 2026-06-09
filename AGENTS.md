# auwsx

Autonomous workspace orchestrator. Sibling to `wsx`; depends on `wsx-core` library crate.

## Session Start

Read the design plan at `~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md` first. Architecture, state machine, CRUD matrix, and verification flow all live there.

## Quick Reference

```bash
cargo build --workspace             # NOT before /sec on dep set
cargo run --bin auwsx               # default = TUI; must be inside tmux
cargo run --bin auwsx -- daemon     # start daemon explicitly
```

## Key Files (planned, mostly stubs at scaffold time)

- `crates/auwsx-core/src/state.rs` — `IssueStatus` enum + scheduler classes + transition matrix
- `crates/auwsx-core/src/pipeline.rs` — one async fn per actionable phase
- `crates/auwsx-core/src/scheduler.rs` — per-project tokio ticker (issues + routines)
- `crates/auwsx-core/src/backlog.rs` — backlog_items CRUD (source/approval) + triage/consolidation
- `crates/auwsx-core/src/steering.rs` — append-only steering into in-flight issues
- `crates/auwsx-core/src/main_jobs.rs` — main-workspace lifecycle, queued ops
- `crates/auwsx-core/src/routines.rs` — cron routines (incl. built-ins: triage, deepsleep, dream, morning-summary)
- `crates/auwsx-core/src/inbox.rs` — `notify` watcher on `~/.auwsx/inbox/*.txt`
- `crates/auwsx-core/src/agent/{claude,codex,opencode}.rs` — `AgentRunner` impls (stubs)
- `crates/auwsx-core/src/ipc.rs` — Unix-socket `Command`/`Response`/`Event` protocol + `serve`/`request`/`EventStream` + unit-testable `dispatch`
- `crates/auwsx-core/src/db/{projects,issues,subtasks,findings}.rs` — typed row structs + CRUD (issues/backlog/steering/findings persistence; `db/mod.rs` re-exports the row types)
- `crates/auwsx-core/src/db/migrations/0001_init.sql` — full schema
- `crates/auwsx-tui/src/cli.rs` — `auwsx` CLI: pure `parse` (arg grammar) + `run_daemon`/`run_request` IPC client glue
- `crates/auwsx-tui/src/app.rs` — ratatui top-level state + view router
- `crates/auwsx-tui/src/input.rs` — keybind table

## Conventions

- Pin deps to exact patch (`=X.Y.Z`); new package requires `/sec` first.
- Tests via `/write-test`, never inline.
- Skills bundled in `skills/`; copied to `~/.claude/skills/` on first run only if missing (never overwrite user copies).
- All filesystem state outside the repo: `~/.local/share/auwsx/state.db`, `~/.auwsx/inbox/`, `<task-worktree>/.auwsx/`.
- No AI attribution in commits or PRs.

- Uncertain about project term/schema/convention/prior decision → `/seek <topic>` first (lightweight KB lookup; same tier as grep/Glob).

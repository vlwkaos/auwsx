# auwsx

Autonomous workspace orchestrator. Schedules coding agents (Claude Code, Codex, opencode) headlessly against a per-project task backlog. Always-on daemon + thin front-ends (ratatui TUI in v0.1, axum/React web in v0.2).

Sibling to [wsx](../wsx). auwsx depends on `wsx-core` (a library crate extracted from wsx) for worktree, tmux, and git primitives.

## Status

**Pre-release.** Scaffold only — no functional implementation yet. See `/Users/vlwkaos/.claude/plans/current-wsx-is-agent-cosmic-gadget.md` for the design plan.

## Layout

```
crates/
  auwsx-core/   shared lib: state machine, pipeline, agent runners, db, scheduler
  auwsx-tui/    v0.1 binary (ratatui)
  auwsx-web/    v0.2 binary (axum + React)
web/            v0.2 React frontend
skills/         bundled skill files (recall, backpressure, commit, memo, dream, deepsleep, gh-pr)
```

## Async / autonomous contract

auwsx is asynchronous. The daemon is always-on; inputs (drafts, followups, feedback, routine edits) are fire-and-forget into SQLite from any of three channels:

1. TUI (v0.1) / Web (v0.2)
2. IPC Unix socket
3. File-watch inbox at `~/.auwsx/inbox/{project}.txt`

Tasks halt at PENDING_FEEDBACK boundaries and wait indefinitely. macOS notifications surface state changes.

See AGENTS.md for the working agreement.

# auwsx — Feature Plan

Backlog only. One line per feature. No checklists.

## IN PROGRESS

(none)

## TODO

### scheduler-ticker
Per-project tokio interval: read issues, dispatch Actionable (≤ max_concurrency, skip already-running), HumanGated wait / soft-gate auto-release on `wait_until`, Terminal archive.

### pipeline-phases
status→role→prompt→spawn→record agent_run per phase. Needs net-new per-phase prompt templates (NOT the stale PRD's) + agent callback env (`AUWSX_SOCK`/`AUWSX_ISSUE_ID`/token) and agent-vs-TUI socket caller scoping.

### worktree-lifecycle
Create at CONSOLIDATING→PLANNING via `wsx_core` (standalone only; delegated reuse target's); teardown at DONE; locking for shared/absorbed worktrees.

### main-jobs-queue
Per-project main-branch serializer so routines never race the pipeline (one main-branch writer at a time).

### routines
Second execution lane (report/idea/knowledge); auwsx owns commit + `writable_paths` scope check; built-ins triage/deepsleep/dream/morning-summary.

### inbox-watcher
`notify` watcher on `~/.auwsx/inbox/*.txt` → backlog_items (source=inbox, auto-approved).

### config-load
Load/save project + daemon config (agent cmds, timeouts, policies, skill_path).

### tui-v0.1
ratatui front-end: views {Overview, Issue, Backlog, Routines, Logs, Config}; keybinds bound to the IPC CRUD matrix.

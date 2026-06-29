# auwsx

Autonomous workspace orchestrator. Sibling to `wsx`; depends on `wsx-core` library crate.

## Session Start

Read the design plan at `~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md` first. Architecture, state machine, CRUD matrix, and verification flow all live there.
If that path is missing, list available plan files with `rg --files /Users/eliot/.claude/plans`, read the closest current auwsx/wsx-agent plan, and report which fallback path was used before making changes.

## Quick Reference

```bash
cargo build --workspace             # NOT before /sec on dep set
cargo run --bin auwsx               # default = TUI (auto-starts daemon if none running)
cargo run --bin auwsx -- daemon     # start daemon explicitly
```

## Command Notes

- Repo-local auwsx file listing without shell redirection: `rg --files .auwsx`
- SQLite string literals inside shell commands need double-quoted SQL, e.g. `sqlite3 '<db path>' "UPDATE issues SET status = 'PLANNING', updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000 WHERE id = 1 AND status = 'CONSOLIDATING';"`
- The issues absorption column is `absorbed_into_id`; active issue inspection can use `sqlite3 '<db path>' "SELECT id, project_id, title, status, substr(COALESCE(description,''),1,120), absorbed_into_id FROM issues WHERE project_id = 1 ORDER BY id;"`
- Project path inspection uses `repo_path`, e.g. `sqlite3 '<db path>' "SELECT id, name, repo_path FROM projects ORDER BY id;"`
- Inline env assignment does not affect same-line shell expansion; use a literal id or `env AUWSX_ISSUE_ID=1 sh -c '"$AUWSX_BIN" issue status "$AUWSX_ISSUE_ID" PLANNING'`.
- Active issue listing from an agent shell should use exported daemon env directly: `"$AUWSX_BIN" issue ls "$AUWSX_PROJECT_ID"`.
- For issue-local `issue get`/`finding add`/`issue status`, use `$AUWSX_BIN`; `target/debug/auwsx` talks to the daemon and can fail on worker-only statuses.
- Cargo test accepts one positional test filter per invocation; run multiple focused tests as separate `cargo test --package <pkg> <filter>` commands.
- This repo has no `crates/auwsx-tui/tests`; broad source/test scans should use existing paths such as `crates/auwsx-core/tests` and `crates/auwsx-tui/src`.
- Shell patterns containing Markdown backticks must be single-quoted, e.g. `rg -n 'on `FAILED`' README.md`; double quotes trigger command substitution.
- Shell patterns beginning with `--` need an option terminator, e.g. `rg -n -- "--arsenal|arsenal_preset_name|codex" crates/auwsx-tui/src/cli.rs`.
- Process inspection should use the plain approved form `ps -axo pid,command`; avoid filtering it with a pipe in this sandbox.
- Package-scoped formatting avoids the sibling `wsx` path dependency: `cargo fmt --package auwsx-core --package auwsx-tui`.
- If the configured current plan path is missing, list available plan files with `rg --files /Users/eliot/.claude/plans`.
- During conflict resolution, restore the default-side worktree copy with `git restore --ours --worktree <paths>`; `git restore --source=:2 --worktree <paths>` does not address conflict stages.
- During non-interactive rebase conflict resolution, continue with `GIT_EDITOR=true git rebase --continue` so Git reuses the existing commit message instead of opening `nvim`.
- On macOS the default DB path is `~/Library/Application Support/auwsx/state.db`, not `~/.local/share/auwsx/state.db`.
- The `agent_runs` start-time column is `spawned_at`; latest run inspection can use `sqlite3 "$HOME/Library/Application Support/auwsx/state.db" "SELECT id, issue_id, role, phase, status_before, status_after, spawned_at, exited_at, exit_kind, exit_code, log_path FROM agent_runs ORDER BY id DESC LIMIT 5;"`.
- The findings review round column is `review_round`, not `round`; latest finding inspection can use `sqlite3 "$HOME/Library/Application Support/auwsx/state.db" "SELECT id, issue_id, review_round, severity, status, title FROM findings ORDER BY id DESC LIMIT 5;"`.
- For review findings, use the literal provided id: `"$AUWSX_BIN" finding accept 3 "..."`; there is no `$AUWSX_FINDING_ID` env var in issue workers.
- ^ Integration workers cannot `git switch main` from an issue worktree when `main` is already checked out in the sibling primary worktree; inspect `/Users/eliot/ws-ps/auwsx` before merging.

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
- `crates/auwsx-core/src/db/arsenal.rs` — global Arsenal presets for reusable per-role agent command templates
- `crates/auwsx-core/src/db/global_settings.rs` — singleton global settings such as persisted Pipeline UX Standard prompt guidance
- `crates/auwsx-core/src/ipc.rs` — Unix-socket `Command`/`Response`/`Event` protocol + `serve`/`request`/`EventStream` + unit-testable `dispatch`
- `crates/auwsx-core/src/db/{projects,issues,subtasks,findings,agent_runs,ask_answers}.rs` — typed row structs + CRUD (issues/backlog/steering/findings persistence + append-only agent run/ask logs; `db/mod.rs` re-exports the row types)
- `crates/auwsx-core/src/db/migrations/0001_init.sql` — full schema
- `crates/auwsx-tui/src/cli.rs` — `auwsx` CLI: pure `parse` (arg grammar) + `run_daemon`/`run_request` IPC client glue
- `crates/auwsx-tui/src/app.rs` — ratatui top-level state + view router
- `crates/auwsx-tui/src/input.rs` — keybind table

## Conventions

- Pin deps to exact patch (`=X.Y.Z`); new package requires `/sec` first.
- Tests via `/write-test`, never inline.
- Implement model first, view second: schema/typed model/IPC before TUI rendering or form behavior.
- TUI colors: reference `ui::theme` roles only — NO inline `ratatui::style::Color::X` anywhere under `crates/auwsx-tui/src/ui/` (except `theme.rs` itself). Add new semantic roles to `theme.rs`; keep `BORDER` distinct from `TEXT_DIM`/`HINT` so chrome never collides with content.
- Skills bundled in `skills/`; copied to `~/.claude/skills/` on first run only if missing (never overwrite user copies).
- All filesystem state outside the repo: `~/.local/share/auwsx/state.db`, `~/.auwsx/inbox/`, `<task-worktree>/.auwsx/`.
- No AI attribution in commits or PRs.

- Uncertain about project term/schema/convention/prior decision → `/seek <topic>` first (lightweight KB lookup; same tier as grep/Glob).

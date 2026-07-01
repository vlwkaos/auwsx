---
slug: good-to-go-axes
kind: coding
title: auwsx good-to-go axes
description: Project-specific verification axes for auwsx changes, including enum/SQL parity, TUI action/view wiring, theme constraints, singleton settings, remote repo config, IPC routes, and prompt-policy safety.
keywords: [good-to-go axes, enum SQL CHECK parity, IssueStatus, AskMode, remote repository config, RemoteProvider, RemoteAuthKind, remote_events status, remote_sync_runs status, TUI action view exhaustiveness, render smoke tests, theme single source of truth, Color grep, cargo test no-run, struct signature change, test target coverage, singleton settings, IPC response coverage, prompt policy, issue-local proxy, TUI capability mismatch]
created: 2026-06-09
modified: 2026-07-01
---

# auwsx good-to-go axes

Project-specific audit axes, extending the skill defaults. Append on each run.

## Enum ↔ SQL CHECK domain parity

Every Rust enum that serializes into a `TEXT` column constrained by a SQL
`CHECK (col IN (...))` must match that domain **exactly, both directions** — a
variant with no CHECK value is a write that fails only at runtime; a CHECK value
with no variant is a row the loader can't parse.

Pairs to keep in lockstep (`src/db/migrations/*.sql` CHECK constraints are the
source of truth):

| Rust | SQL column |
|------|-----------|
| `state::IssueStatus` (SCREAMING_SNAKE) | `issues.status` |
| `main_jobs::MainJobStatus` (SCREAMING_SNAKE) | `main_jobs.status` |
| `backlog::Source` (snake) | `backlog_items.source` |
| `backlog::Approval` (snake) | `backlog_items.approval` |
| `steering::SteeringSource` (snake) | `steering.source` |
| `db::projects::MergeMode` (snake) | `projects.merge_mode` |
| `db::projects::CompletionPolicy` (snake) | `projects.completion_policy` |
| `db::findings::Severity` (snake) | `findings.severity` |
| `db::findings::FindingStatus` (snake) | `findings.status` |
| `db::agent_runs::Role` (snake) | `agent_runs.role` |
| `agent::ExitKind` (snake) | `agent_runs.exit_kind` |
| `db::ask_answers::AskMode` (snake) | `ask_answers.mode` |
| `db::remote::RemoteProvider` (snake) | `project_remote_configs.provider`, `remote_issue_links.provider`, `remote_pr_links.provider`, `remote_events.provider` |
| `db::remote::RemoteAuthKind` (snake) | `project_remote_configs.auth_kind` |
| `db::remote::RequiredChecksPolicy` (snake) | `project_remote_configs.required_checks_policy` |
| `db::remote::RemotePrState` (snake) | `remote_pr_links.state` |
| `db::remote::RemoteEventStatus` (snake) | `remote_events.status` |
| `db::remote::RemoteSyncDirection` (snake) | `remote_sync_runs.direction` |
| `db::remote::RemoteSyncKind` (snake) | `remote_sync_runs.kind` |
| `db::remote::RemoteSyncStatus` (snake) | `remote_sync_runs.status` |

These enums use hand-rolled `as_str` plus parser helpers (NOT serde) for the SQL
bind, mirroring `IssueStatus`. `tests/crud.rs` proves parity at runtime with a
positive-control insert per enum (a valid `as_str()` value must pass the CHECK)
plus an `as_str()` parser round-trip — so a drift fails a test, not just the
diff below.

Check:
```bash
rg -oN '=> "[A-Z_]+"' crates/auwsx-core/src/state.rs | rg -o '[A-Z_]+' | sort -u > "$TMPDIR/enum.txt"
sed -n '/status TEXT NOT NULL CHECK (status IN (/,/))/p' crates/auwsx-core/src/db/migrations/0001_init.sql \
  | rg -o "'[A-Z_]+'" | tr -d "'" | sort -u > "$TMPDIR/sql.txt"
comm -3 "$TMPDIR/enum.txt" "$TMPDIR/sql.txt"   # any output = drift = FAIL
```
The `state.rs` test `given_each_variant_when_as_str_then_matches_spec_id` plus
the `tests/db_smoke.rs` CHECK-rejection cases guard this, but they encode the
domain by hand — re-run the diff above whenever either side changes.

## Module-rename doc sync

`CLAUDE.md` is a symlink to `AGENTS.md` (one file). When a `crates/.../src`
module is renamed (e.g. `drafts.rs`→`backlog.rs`, `followups.rs`→`steering.rs`),
update the `## Key Files` list in `AGENTS.md` — both the path and its one-line
description. The design plan (`~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md`)
is the architecture PRD; it lags the code intentionally during a model pivot and
is synced as its own task, not gated here.

## TUI action/view exhaustiveness

The ratatui front-end (`auwsx-tui/src/{input,app,ui}`) has two closed sets that
must stay fully wired — a key binding with no handler is a dead key; a `View`
with no renderer panics the match:

- Every `input::Action` variant must be **produced** by `input::map_key` AND
  **consumed** by `app::App::apply` (no orphan affordance).
- Every `app::View` in `View::ORDER` must have an arm in `ui::draw`'s view match
  AND in the footer-hint match.

Check:
```bash
rg -n 'Action::' crates/auwsx-tui/src/app.rs        # consumed set
rg -n 'Action::\w+' crates/auwsx-tui/src/input.rs   # produced set
rg -n 'View::\w+ =>' crates/auwsx-tui/src/ui/mod.rs # both draw + footer arms
```
Guarded at runtime by the `ui/mod.rs` render-smoke tests (draw every view at
normal + tiny size, no panic) and the `input.rs` map_key tests. A missing draw
arm fails to compile (non-exhaustive match), but a missing footer arm or an
orphan Action does NOT — so eyeball the produced/consumed sets on UI changes.

## TUI theme single-source-of-truth

All TUI colors live in `auwsx-tui/src/ui/theme.rs` as named semantic roles
(`BORDER`, `TEXT`, `TEXT_DIM`, `HINT`, `ACCENT`, `OK`/`WARN`/`ERR`,
`TREE_CONNECTOR`, `HIGHLIGHT_FG`) plus `border()`/`title()`/`dim()`/`hint()`/
`highlight()` style helpers. NO inline `ratatui::style::Color::X` anywhere under
`ui/` except `theme.rs` itself — inline colors are how the dim-hint and
border-equals-content regressions crept in originally. Contrast invariant:
`BORDER` must differ from `TEXT_DIM`/`HINT`/`TEXT` so chrome never collides with
content.

Check:
```bash
rg -n 'Color::' crates/auwsx-tui/src/ui/ -g '!theme.rs'   # any hit = FAIL
```
Guarded by `theme.rs` unit tests (the `BORDER != TEXT_DIM/HINT/TEXT` asserts),
but a NEW inline `Color::` in another `ui/` file is NOT caught by any test or
compiler check — only this grep catches it. The rule is also stated in
`AGENTS.md`, but enforcement is manual: run the grep on any `ui/` change.

## Struct or enum shape changes must build tests

`cargo build` does not compile test-only constructors. After changing a public
test-constructed struct or IPC enum variant such as `NewProject` or
`Command::AddProject`, run at least:

```bash
cargo test --package auwsx-core --package auwsx-tui --no-run
```

The failure pattern is Rust `E0063` in tests after adding a field that normal
builds miss. Full `cargo test --package auwsx-core --package auwsx-tui` remains
the preferred final check.

## Project command-source contract

When project agent command configuration changes, test both persisted source and
runtime resolution. A project may store an Arsenal preset reference plus blank
or partial per-role overrides; runtime must resolve effective commands from
override first, then preset.

Required checks:

- CRUD tests prove blank overrides resolve to Arsenal commands.
- CRUD tests prove later Arsenal edits change linked projects' effective
  commands.
- CRUD tests prove nonblank project overrides win over Arsenal.
- TUI tests prove Settings opens linked projects with Arsenal selected and keeps
  nonblank override fields visible.

## Worktree lifecycle reset-collision coverage

When worktree creation, branch naming, cleanup, or DB reset behavior changes,
test the case where SQLite no longer knows about an auwsx issue but git still
has `auwsx/issue-N` in its branch/worktree registry. Issue ids can be reused
after a DB reset, so branch creation must not fail silently or destroy live
work.

Required checks:

- A prunable stale worktree for `auwsx/issue-N` is pruned/archived and a new
  worktree for the current issue is created.
- A live checked-out `auwsx/issue-N` worktree is refused, not overwritten.
- Any pre-agent setup failure records a run/log entry with the concrete cause
  before the issue is marked `FAILED`.

## Singleton settings and IPC response coverage

When adding a singleton config table or a new IPC `Response` variant, verify the
entire route, not just the database helper:

- Migration creates the singleton row, with a smoke test that queries the row.
- IPC dispatch exposes both read and write commands when the value is editable.
- CLI response printing has an arm for the new `Response` variant.
- TUI refresh state and render path handle missing/not-yet-loaded state without
  panicking.

## Prompt policy and control-channel safety

When global prompt/profile guidance or issue-local control changes, verify the
public boundary rather than only helper functions:

- Issue-local socket/proxy commands are filtered by the same issue allowlist as
  the control outbox, or `AUWSX_SOCK` is absent from issue workers.
- Regression test: issue-local control cannot call `UpdateGlobalSettings`.
- Persisted guidance has a max length, rows-affected checks, and tests.
- Prompt guidance block is delimited and says it cannot bypass controls, reveal
  secrets, or override system/developer/repo instructions.
- CLI printing strips or escapes ASCII control characters from persisted text.

## TUI capability and typed-form assertions

Capability-driven UI must not advertise actions that the selected row cannot
perform. Check that read-only config rows do not show Enter edit/open, hidden
project actions such as `p` are gated, and sectioned typed forms preserve
select/completion behavior. Prefer focused buffer assertions for footer/action
labels over broad render-only smoke tests.

## Agent decision run observability

When a scheduler phase adds a new agent-mediated decision path outside normal
issue `agent_runs`, verify the decision has its own durable run record and that
all fallback paths close that row with an inspectable reason. The test must cover
invalid agent output and executor-level failure, not only successful decisions.

## IPC model compatibility during development

When adding a field to any type serialized over IPC (`Command`, `Response`
payload structs such as `Project`, config preset rows, events), decide whether a
new client may see an old daemon response or a new daemon may receive an old
client command. For additive fields, prefer `#[serde(default)]` plus boundary
normalization in dispatch, and add a legacy JSON regression test. Otherwise add
an explicit protocol-version/restart path so stale daemons do not surface raw
`missing field` serde errors.

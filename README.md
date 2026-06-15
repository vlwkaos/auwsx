# auwsx

Autonomous workspace orchestrator. auwsx runs coding agents headlessly against a
per-project backlog, with an always-on daemon as the executor and thin clients
for observing and issuing commands.

Sibling to [wsx](../wsx). auwsx depends on `wsx-core` for worktree, tmux, and
git primitives.

## Status

Pre-release, but no longer scaffold-only. The current implementation includes:

- SQLite-backed projects, backlog items, issues, steering, findings, subtasks,
  routines, main jobs, scheduler runs, and agent runs.
- A daemon process with Unix-socket IPC. Database writes are daemon-owned.
- A ratatui operator console with one project tree and contextual detail pane.
- A deterministic issue pipeline driven by issue status.
- Automatic scheduler ticks per project, plus immediate manual run commands.
- Routine execution through the main-job lane.

## Running

```bash
cargo run --bin auwsx -- daemon
cargo run --bin auwsx
```

The default binary starts the TUI. The daemon can also be started explicitly and
kept running while the TUI reconnects as a client.

## Scheduler Cadence

Each project has `schedule_interval_min`:

- `None` means manual-only. The daemon will not pick up backlog automatically.
- `Some(0)` means run this project on every daemon tick.
- `Some(n)` means run after at least `n` minutes since the last automatic tick.

Manual commands bypass the cadence gate:

- Project selected in the TUI: `E` runs one scheduler pass for that project.
- Backlog item selected: `E` approves/promotes it if needed, then runs the new
  issue's first actionable phase.
- Issue selected: `E` runs the current actionable phase if it is not already
  running.
- Routine selected: `E` enqueues and runs one main job for that routine.

The detail pane shows recent scheduler ticks, their source (`auto` or
`manual`), backlog counts, and picked decisions. A pending backlog item is not
automatic work until it is approved; human and inbox backlog default to approved,
agent and routine backlog default to pending.

## TUI Shape

The TUI is an operator console:

- Left pane: a tree of all registered projects; each project expands to its
  Routines, Backlog, and Issues. `⏎` toggles a project's children.
- Right pane: contextual detail for the highlighted tree row.
- Bottom bar: mode, key hints, last result/error, and the running version.

Useful keys:

- `a`: add project.
- `c`: edit the selected project config.
- `n`: add backlog, or add a routine when the routines section is selected.
- `e`: edit the selected backlog item or routine.
- `a`: approve selected backlog item or gated issue.
- `x`: dismiss selected backlog item.
- `T`: triage approved backlog for the selected project.
- `E`: execute the selected project/backlog/issue/routine now.
- `Space`: enable or disable the selected routine.
- `⏎`: expand/collapse the selected project, or open the selected issue.
- `f`: add steering to the selected issue.
- `l`: refresh/toggle issue log tail.
- `Tab` / `BackTab`: switch compatibility views.

Project config is editable through daemon IPC. The TUI never opens SQLite
directly.

## Pipeline

The pipeline is split between deterministic orchestration and focused agent
instructions.

Deterministic parts:

- Scheduler cadence and concurrency checks.
- Backlog approval, triage, and consumed-item tracking.
- Issue status legality and soft gates.
- Worktree creation/teardown.
- Agent run, main job, scheduler run, and log-path recording.

Instruction parts:

- Per-phase prompts generated from issue/project state.
- The subprocess agent command configured on the project.
- Agent-authored transitions via the auwsx control CLI.

Typical issue flow:

```text
CONSOLIDATING -> PLANNING -> PLANNED -> IMPLEMENTING -> REVIEW
-> AUDIT / NEEDS_FIX / CONFLICTED -> ENDED -> COMPLETING -> DONE
```

`PLANNED` and, depending on completion policy, `ENDED` are human or soft gates.
The scheduler only runs statuses that are actionable and never spawns the same
issue twice.

## Layout

```text
crates/
  auwsx-core/   shared lib: state machine, pipeline, agent runners, db, scheduler
  auwsx-tui/    ratatui client binary
  auwsx-web/    placeholder web binary
skills/         bundled skill files
```

All persistent runtime state lives outside the repo by default:

- `~/.local/share/auwsx/state.db`
- `~/.local/share/auwsx/runs/`
- `~/.local/share/auwsx/main-jobs/`
- `~/.auwsx/inbox/`

`AUWSX_DATA_DIR` and `AUWSX_SOCK` can override artifact and socket locations.

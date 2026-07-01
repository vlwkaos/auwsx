# auwsx

Autonomous workspace orchestrator. auwsx runs coding agents headlessly against a
per-project backlog, with an always-on daemon as the executor and thin clients
for observing and issuing commands.

Sibling to [wsx](../wsx). auwsx depends on `wsx-core` for worktree, tmux, and
git primitives.

## Status

Pre-release, but no longer scaffold-only. The current implementation includes:

- SQLite-backed projects, backlog items, issues, steering, findings, subtasks,
  routines, main jobs, scheduler runs, route runs, agent runs, and global
  Arsenal presets.
- A daemon process with Unix-socket IPC. Database writes are daemon-owned.
- A ratatui operator console with one project tree and contextual detail pane.
- A thin `auwsx-web` GitHub webhook adapter for inbound remote `/auwsx-run`
  comments.
- A deterministic issue pipeline driven by issue status.
- Automatic scheduler ticks per project, plus immediate manual run commands.
- Routine execution through the main-job lane.

## Running

```bash
cargo run --bin auwsx -- daemon
cargo run --bin auwsx
cargo run --bin auwsx-web
```

The default binary starts the TUI. The daemon can also be started explicitly and
kept running while the TUI reconnects as a client.

`auwsx-web` listens on `AUWSX_WEB_ADDR` or `127.0.0.1:7789` and connects to the
daemon over the normal `AUWSX_SOCK` path. GitHub issue-comment webhooks should
target `POST /webhooks/github`. If a project remote config has
`webhook_secret_ref`, that value is read as an environment variable name
(`env:NAME` is also accepted) and verified against `X-Hub-Signature-256`.

When a project uses PR merge mode, approving a ready issue queues a remote PR
sync instead of local `MERGING`. Later scheduler ticks observe linked PRs; when
GitHub reports the PR as merged, auwsx updates the remote PR link, records an
inbound PR sync audit row, and marks the local issue `DONE`.

If SQLite state is reset outside the daemon, git may still have auwsx-managed
issue worktrees registered. Prune those orphaned worktrees before reusing the
repo:

```bash
cargo run --bin auwsx -- worktree prune /path/to/repo
```

Issue worktree cleanup policy:

- `DONE` and `ABANDONED` issues are cleaned by the scheduler; their
  `branch`/`worktree_path` fields are cleared after teardown.
- `FAILED` keeps its worktree for inspection. Retry it after fixing the cause,
  or remove the worktree explicitly when done:

```bash
cargo run --bin auwsx -- issue retry <issue_id>
cargo run --bin auwsx -- issue cleanup <issue_id>
```

On daemon startup, auwsx closes interrupted issue/main-job run rows left by the
previous daemon and marks the affected work `FAILED` so it does not respawn
silently.

## Project Onboarding

When a project is registered or updated and `repo_path` points at a git repo,
auwsx creates or appends an idempotent `AGENTS.md` block with editable knowledge
collection hints:

- coding language collections inferred from files such as `Cargo.toml`,
  `package.json`, `pyproject.toml`, `go.mod`, or `Package.swift`
- project domain from the auwsx project name
- knowledge domains inferred from `knowledge/coding`, `knowledge/domain`, and
  `knowledge/history`

The block is marked with `<!-- auwsx:knowledge-collections -->`; if that marker
already exists, auwsx leaves the file alone.

## Scheduler Cadence

Each project has `schedule_cron` as the user-facing scheduler cadence:

- Blank, `manual`, or `@manual` means manual-only.
- `@tick` means run this project on every daemon tick.
- Five-field cron such as `*/30 * * * *` or `15 9 * * 1-5` runs on matching
  local wall-clock minutes.
- Shorthand is accepted and normalized to cron where exact: `30m`, `1h`,
  `1d`, `7d`. Durations that are not exact five-field cron, such as `90m`,
  are stored as `@every 90m`.

The legacy `schedule_interval_min` column is still read as a fallback for old
databases and scripts. New UI and CLI writes should use `schedule_cron`.

Manual commands bypass the cadence gate:

- Project selected in the TUI: `E` runs one scheduler pass for that project.
- Backlog item selected: `E` approves/routes it if needed, then runs the target
  issue's next actionable phase.
- Issue selected: `E` runs the current actionable phase if it is not already
  running; on `FAILED`, it retries from the last actionable phase.
- Routine selected: `E` enqueues and runs one main job for that routine.

The project detail pane shows a board with broad lanes (`TODO`, `IN PROGRESS`,
`REVIEW`, `COMPLETE`) while each issue still carries its detailed scheduler
status. A pending backlog item is not automatic work until it is approved; human
and inbox backlog default to approved, agent and routine backlog default to
pending. Approved backlog is semantically routed: if it clearly belongs to an
existing queue-capable issue, auwsx appends a consolidation queue message and
links the backlog row to that issue; otherwise it creates a new issue. Ambiguous
or failed route-agent output falls back to creating a new issue. Route decisions
are recorded in `routing_runs`.

## Memory Interface

auwsx prompts do not hardcode a private memory system. Workers call stable
auwsx-owned memory skills, and the selected Memory provider decides how those
operations are implemented.

| Operation | Purpose |
| --- | --- |
| `memory-retrieve` | Load relevant project/domain/session context |
| `memory-save` | Persist durable progress, decisions, and results |
| `memory-consolidate --mode dream` | Consolidate session memory into durable knowledge |
| `memory-consolidate --mode deepsleep` | Audit and housekeep durable knowledge |

Codex-facing prompts use `$memory-save`; slash-skill agents use `/memory-save`.
Those names are the public contract. Provider-specific tools such as `ir`,
`recall`, `memo`, `dream`, and `deepsleep` stay behind the provider boundary.

Memory configuration is preset-backed, like Arsenal. The global setting selects
a Memory preset, and each preset wires four interfaces: `retrieve`, `save`,
`dream`, and `deepsleep`.

Built-in presets:

- `portable-markdown`: internal auwsx markdown memory under the auwsx data
  directory. It works without `ir` or a personal vault.
- `auwsx-skills`: retrieves through `seek.sh`, saves session artifacts under
  `knowledge/sessions/auwsx`, exposes `dream` setup/detection, and runs the
  packaged `deepsleep-audit.sh` for housekeeping. Project `skill_path` or
  `AUWSX_SKILL_PATH` can point at that skill stack.

Custom presets can be command-backed:

```bash
auwsx memory preset set my-memory \
  --retrieve-kind command --retrieve-cmd 'my-retrieve {query}' \
  --save-kind command --save-cmd 'my-save {content_file}' \
  --dream-kind command --dream-cmd 'my-dream {project_root}' \
  --deepsleep-kind command --deepsleep-cmd 'my-deepsleep {project_root}'
```

Available command placeholders: `{query}`, `{kind}`, `{content_file}`,
`{mode}`, `{project_id}`, `{project_root}`, `{skill_root}`, `{memory_dir}`.
Missing provider scripts or failing commands are reported as setup errors.

Routine routes are:

- `report`: keep the run report/history only.
- `backlog`: produce backlog candidates; routine-authored backlog stays pending.
- `memory`: use memory operations only. It is not permission to edit source
  files or bypass the issue pipeline.

Project config owns the deepsleep cadence field as `deepsleep_cron`. Blank,
`manual`, or `@manual` disables the project-owned memory routine. If deepsleep
has never run for the project, the first automatic tick runs it immediately;
after that, the cron cadence controls subsequent runs. The legacy
`deepsleep_interval_days` column remains a compatibility fallback.

## Real-Agent E2E

The ignored LLM e2e test registers its temporary project with the built-in
`codex` Arsenal preset by default. `AUWSX_E2E_AGENT_CMD` is only a test-harness
override for comparing another command template; it is not runtime config.

```bash
cargo test --package auwsx-tui --test local_merge_e2e configured_llm_agent_can_drive_one_issue_to_terminal -- --ignored --nocapture
```

## TUI Shape

The TUI is an operator console:

- Left pane: a tree of all registered projects; each project expands to its
  Routines, Backlog, and Issues. Expanded projects show counts in those child
  rows; collapsed project rows show compact `R/B/I` counts.
- Right pane: contextual detail for the highlighted tree row. Embedded issue
  detail labels the issue plan checklist, review findings, queue messages, phase
  reports, and latest agent log tail in a compact readable form. When issue log
  focus is active, `k` scrolls older log lines, `j` returns toward the newest
  lines, `PgUp`/`PgDn` page, and `Home`/`End` jump to oldest/newest.
- Ask view: project-level Q&A history. Each ask runs once with current project
  status plus either `recall` or `seek` mode, then stores the answer newest-first
  for later reading/copying.
- Bottom bar: context-aware key hints, last result/error, and the running
  version. Project detail shows `last tick` as a local readable timestamp.

Useful keys:

- `p`: register a project repository.
- `n`: add backlog, add a routine from the routines section, or add a queue
  message on an eligible active issue.
- `e`: edit the selected project, backlog item, routine, Arsenal preset, or
  global pipeline UX guidance when that context is editable.
- `a`: approve a backlog item or toggle a routine.
- `d`: dismiss backlog, delete routine, unregister project, abandon active
  issue, or archive/cleanup terminal issue worktree state.
- `E`: execute the selected project/backlog/issue/routine now.
- `S`: open global Settings.
- `m`: move project order/profile when a project row is selected.
- `⏎`: enter project kanban, toggle section roots, open issue detail, or edit
  the selected Settings row.
- `?`: ask a one-shot project question.
- `Tab` / `BackTab`: switch compatibility views.
- In issue log focus: `k` / `j` scroll the agent log, `PgUp` / `PgDn` page it,
  and `Home` / `End` jump oldest/newest.

Project config is edited from the project row with `e`; Arsenal is the primary
agent-command choice and per-role command fields are overrides when a preset is
selected. Settings is global-only: profiles, Arsenal presets, prompt catalog
review, and persisted pipeline UX guidance. The TUI never opens SQLite directly.

## Pipeline

The pipeline is split between deterministic orchestration and focused agent
instructions.

Deterministic parts:

- Scheduler cadence and concurrency checks.
- Backlog approval, triage, and consumed-item tracking.
- Route-agent decisions and fallback reasons for approved backlog.
- Issue status legality and soft gates.
- Phase entry transitions before a subprocess is spawned.
- Worktree creation/teardown.
- Agent run, phase report, main job, scheduler run, and log-path recording.

Instruction parts:

- Per-phase prompts generated from issue/project state.
- The subprocess agent command configured on the project.
- Agent-authored transitions via the auwsx control CLI.

Typical issue flow:

```text
NEW -> PLANNING -> PLAN_READY -> WORKING -> REVIEWING
-> AUDITING / FIXING -> READY_TO_MERGE -> MERGING -> DONE
                         \-> WORKING
                         \-> RESOLVING_CONFLICT -> CONFLICT_BLOCKED
```

`PLAN_READY` and, depending on completion policy, `READY_TO_MERGE` are human or
soft gates. The scheduler only runs statuses that are actionable and never
spawns the same issue twice. `NEW` is an eligibility marker; the pipeline enters
`PLANNING` before the planner prompt runs.

Every agent phase writes `.auwsx/phase-report.md` before setting its next
status. auwsx snapshots that file onto the `agent_runs` row for that phase, so
the Issue screen can show how planning, implementation, review, audit, conflict
resolution, and merge each reached their result. The older issue-level
`plan.md`, `progress.md`, and `human-verify.md` files remain the rolled-up issue
summary surfaces.

`READY_TO_MERGE` defaults to a manual human verification gate. A human can add a
queue message while the issue is parked there, then move it back to `WORKING` for
more work on the same branch. Before an issue reaches `READY_TO_MERGE`, the
agent writes `.auwsx/human-verify.md` with concise app-run commands and pass/fail
checks for the operator.

Merge approval is explicit under manual policy:

```bash
auwsx issue merge <issue_id>    # release one READY_TO_MERGE issue
auwsx project merge <project_id> # release all READY_TO_MERGE issues oldest-first
```

In the TUI, press `E` on a `READY_TO_MERGE` issue to approve that issue, or on a
project row to approve the project's ready merge queue. Local merge mode still
runs one merge at a time and pauses later merges behind `CONFLICT_BLOCKED`.
Project config `merge_delay` is only used by `completion=soft`: it is the number
of minutes to wait before auto-releasing `READY_TO_MERGE` to `MERGING`.

Press `S` in the TUI to open Settings. Settings is a structured navigator for
global runtime defaults, profiles, Arsenal presets, the Prompt Catalog, and
persisted Pipeline UX Standard guidance. Arsenal presets and the Pipeline UX
Standard are editable there. Project-specific merge, schedule, concurrency,
timeout, and skill-path settings stay on the project row edit form. Prompt
Catalog shows every generated auwsx phase prompt from the same builder the
daemon uses, so prompt review does not require waiting for a live agent run.

## Layout

```text
crates/
  auwsx-core/   shared lib: state machine, pipeline, agent runners, db, scheduler
  auwsx-tui/    ratatui client binary
  auwsx-web/    placeholder web binary
skills/         bundled skill files
```

All persistent runtime state lives outside the repo by default, under the
platform data directory:

- macOS: `~/Library/Application Support/auwsx/state.db`
- macOS: `~/Library/Application Support/auwsx/runs/`
- macOS: `~/Library/Application Support/auwsx/main-jobs/`
- XDG/fallback: `~/.local/share/auwsx/...`
- `~/.auwsx/inbox/`

`AUWSX_DATA_DIR` and `AUWSX_SOCK` can override artifact and socket locations.

Agent command templates are plain argv strings, not shell scripts. `{prompt}` is
substituted as one argument, or piped on stdin when omitted. `{auwsx_socket_dir}`
expands to the daemon socket directory. Issue-worker commands that need access
to the replayed control outbox directory should use `{auwsx_control_dir}`.

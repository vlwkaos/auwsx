---
slug: architecture-overview
kind: coding
title: auwsx architecture overview (daemon + thin clients)
description: System architecture — a long-lived daemon owns SQLite, scheduler, pipeline, subprocess agent runners, IPC, inbox, and routines; thin clients (CLI/TUI/web) talk over a Unix socket; wsx-core consumed as a path dep.
keywords: [daemon owns sqlite scheduler pipeline runners ipc, thin clients unix socket, auwsx daemon, control CLI agent vs TUI, wsx-core path dependency, two execution lanes issues routines, main queue main_jobs, skills injection inline per phase, routines report idea knowledge, status as sync marker, subprocess agent not tmux, architecture overview]
created: 2026-06-09
modified: 2026-06-09
---

# auwsx architecture overview

> Reconciled to the 2026-06-09 issue model. The 2026-06-04 architecture notes
> (tmux agents, task pipeline, detached-pane completion) are SUPERSEDED — kept
> below only as a dated historical note.

## Repo split

Two-repo split: `wsx` (sibling at `/Users/vlwkaos/ws/wsx`) holds the
worktree+tmux+git primitives in a `wsx-core` library crate. `auwsx` consumes
`wsx-core` via **path dep** (`../wsx/crates/wsx-core`), declared in both the root
`workspace.dependencies` and `crates/auwsx-core/Cargo.toml`. Since wsx 0.16.0
(crates/ layout merged to wsx main), any wsx checkout satisfies the path dep — no
crates.io publish, no git URL, no branch coupling. If wsx moves, update both
Cargo.toml references in lockstep.

## Daemon-first

A long-lived **`auwsx daemon`** owns: SQLite, the scheduler, the pipeline, the
**direct-subprocess** agent runners, IPC, the file-watch inbox, routines, and the
notification emitter. Front-ends (CLI, TUI v0.1, web v0.2) are **thin clients** —
observers + command issuers over a JSON-lines Unix socket (see
coding/ipc-protocol.md). One command surface, two callers (agent control CLI vs
human TUI), scoped per-caller.

## Core principles

- **Status as sync marker** (see domain/issue-model.md): the scheduler is a
  ticker, not a process tracker. Crash-resume is free.
- **No tmux for agents**: agents are direct `tokio::process` children auwsx owns
  (see coding/agent-subprocess-runner.md). tmux is human-only spectating.
- **auwsx never parses agent prose**: agent→auwsx is the validated control CLI.
- **Two execution lanes**: the issue pipeline + the routines lane. Routines
  serialize through the per-project **main queue** (`main_jobs`) so they never
  race the pipeline (one main-branch writer at a time).

## Routines lane

| type | writes | env |
|------|--------|-----|
| report | nothing | disposable worktree |
| idea | backlog only (via `auwsx draft add` → pending) | disposable worktree |
| knowledge (/dream, /deepsleep) | only inside configurable `writable_paths` | main worktree |

auwsx owns the commit + path-scope check: routine agent edits in worktree and
signals done; auwsx verifies diff ⊆ `writable_paths` BEFORE committing;
out-of-scope → `main_jobs.status=REJECTED` + flagged. Agent never pushes.
`/dream` is BOTH a post-merge pipeline step AND a scheduled routine; `/deepsleep`
is routine-only.

## Skills injection

auwsx-owned editable skill path (default `~/.local/share/auwsx/skills`, override
`projects.skill_path`), seeded from bundle on init, INDEPENDENT of
`~/.claude/skills`. Mechanism = per-phase **inline** of the skill text →
agent-agnostic (claude/codex/opencode identical). Core: recall, backpressure,
simplify, good-to-go, write-test, commit, memo. KB skills (recall/memo/dream/
deepsleep) gated on `ir` presence with graceful degrade.

## Filesystem state (outside repo)

`~/.local/share/auwsx/state.db`, `~/.auwsx/inbox/`, `<worktree>/.auwsx/`.

## Historical note (2026-06-04, SUPERSEDED)

The original design had agents typed into detached tmux panes with
signal-done/pane-poll completion, and a `task` pipeline
(QUEUED→PREPARING→ITERATING→QA→…→DONE) with drafts/followups entities. Replaced
2026-06-09 by the issue model, direct subprocess runners, backlog (was drafts),
and steering (was followups).
